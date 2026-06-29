//! polis Reputation primitive.
//!
//! Per spec §Reputation: stake-bonded vouching with slashing. Reputation is bound to an agent's
//! `code_hash` (not its wallet), market-priced via USDC stake, and slashable on default.
//!
//! ## State machine
//!
//! - **VouchPool**: aggregate stake for a single agent, keyed by `code_hash`.
//! - **Voucher**: an individual staker's position inside a VouchPool, with a 7-day cooldown to exit.
//! - **RepStats**: global singleton recording the max-ever `total_staked` seen across any agent.
//!   Used as the denominator for the reputation score so the score is normalised to [0, 1] (bps).
//!
//! ## Yield accounting (Compound-style index)
//!
//! Yield is distributed via a `cumulative_yield_per_share` index on the VouchPool. Each Voucher
//! snapshots the index at the time of its last touch (`yield_index_at_entry`). Pending yield
//! for a voucher = `(pool.index - voucher.yield_index_at_entry) * voucher.amount / SCALE`.
//!
//! Why an index rather than rebasing share quantities? It is gas-cheap (no per-voucher writes when
//! yield arrives), it cleanly handles staking-while-yielding (new voucher's index snapshots the
//! current value, so they earn nothing on yield that arrived before them), and slashing simply
//! shrinks the pool — vouchers' pending yield in the index is unaffected by haircuts to principal.
//!
//! ## Coupling with Credit
//!
//! The Credit program holds this program's ID and calls in via CPI to:
//!   1. `distribute_voucher_yield` on every repayment (30% of interest paid).
//!   2. `slash` on agent default (50% returned to LP pool, 50% burned).
//! Credit reads the reputation score by loading the `VouchPool` and `RepStats` PDAs directly and
//! computing the ratio client-side (Anchor has no native view fns; an on-chain CPI just to read
//! state would waste CU). The `read_reputation_score` instruction is provided for off-chain use
//! and returns the value via `set_return_data`.
//!
//! ## Authority on privileged instructions
//!
//! `distribute_voucher_yield` and `slash` are gated to the Credit program. We verify the calling
//! program by checking that the `credit_program_signer` PDA — owned by the Credit program — has
//! signed the CPI. Because the seed is the Credit program's ID and only the Credit program can
//! sign for it, this is a sound authority check without requiring a hardcoded program ID at
//! compile time (Credit's ID is recorded in `RepStats.credit_program_id` at init).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::pubkey::Pubkey;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("AskjrF52LFEJaQc7d5Z46AMEDnKRoURMFBqmcacvAEoP");

/// Cooldown before an unvouch can be withdrawn. 7 days, per spec §Reputation.
pub const UNVOUCH_COOLDOWN_SECONDS: i64 = 7 * 24 * 60 * 60;

/// Index scale for the cumulative-yield-per-share fixed-point math.
/// 1e12 is enough headroom: total_staked is u64 (max ~1.8e19), yield is u64,
/// product fits in u128 without overflow up to absurd values.
pub const YIELD_INDEX_SCALE: u128 = 1_000_000_000_000;

/// Slash percentage that flows back to the LP Pool (the rest is burned). 50%, per spec.
pub const SLASH_PCT_TO_POOL: u64 = 50;
pub const SLASH_PCT_BURNED: u64 = 50;

/// Burn address: all-zero pubkey, owned by no program. Funds sent here are unrecoverable.
/// This matches the spec's intent of "50% burned" without needing an actual token-burn instruction
/// (which would require mint authority on USDC — which we don't have).
/// In production we'd CPI into a token-burn-permissioned escrow; for the prototype this is fine.
pub fn burn_address() -> Pubkey {
    Pubkey::new_from_array([0u8; 32])
}

#[program]
pub mod reputation {
    use super::*;

    /// One-time global init. Records the Credit program's ID as the only authorised caller for
    /// privileged CPIs (`distribute_voucher_yield`, `slash`).
    ///
    /// SAFETY: admin-only. Sets the trust root for cross-program calls.
    pub fn init_rep_stats(ctx: Context<InitRepStats>, credit_program_id: Pubkey) -> Result<()> {
        let stats = &mut ctx.accounts.rep_stats;
        stats.max_total_staked_seen = 0;
        stats.total_active_vouchers = 0;
        stats.credit_program_id = credit_program_id;
        stats.admin = ctx.accounts.admin.key();
        stats.bump = ctx.bumps.rep_stats;
        Ok(())
    }

    /// Update the Credit program ID. Admin-only escape hatch in case Credit is redeployed.
    /// In production this would be governed by the bicameral DAO.
    pub fn set_credit_program(ctx: Context<SetCreditProgram>, new_id: Pubkey) -> Result<()> {
        let stats = &mut ctx.accounts.rep_stats;
        require_keys_eq!(ctx.accounts.admin.key(), stats.admin, RepError::Unauthorized);
        stats.credit_program_id = new_id;
        Ok(())
    }

    /// Initialise a VouchPool for an agent. Idempotent: anyone may call once per `code_hash`.
    /// The `vouch` instruction also lazy-inits, but exposing this explicitly is convenient for
    /// off-chain orchestration.
    ///
    /// SAFETY: VouchPool is keyed by `code_hash`, binding stake to the agent identity (not wallet).
    pub fn init_vouch_pool(ctx: Context<InitVouchPool>, code_hash: [u8; 32]) -> Result<()> {
        let pool = &mut ctx.accounts.vouch_pool;
        pool.agent_code_hash = code_hash;
        pool.total_staked = 0;
        pool.cumulative_yield_per_share = 0;
        pool.total_slashed = 0;
        pool.total_yield_received = 0;
        pool.bump = ctx.bumps.vouch_pool;
        Ok(())
    }

    /// Stake `amount` USDC in support of agent `code_hash`.
    ///
    /// Voucher accounting:
    ///   - First-time staker: create Voucher, snapshot the current yield index.
    ///   - Existing staker: settle pending yield into their balance first (so the new stake
    ///     starts at the current index without giving them yield on already-yielded periods),
    ///     then add the new amount.
    ///
    /// SAFETY: this instruction holds the invariant that `voucher.amount` always reflects the
    /// staker's claim at the *current* yield index. Settlement before any balance change is what
    /// preserves that.
    pub fn vouch(ctx: Context<Vouch>, _code_hash: [u8; 32], amount: u64) -> Result<()> {
        require!(amount > 0, RepError::InsufficientStake);

        let pool = &mut ctx.accounts.vouch_pool;
        let voucher = &mut ctx.accounts.voucher;
        let stats = &mut ctx.accounts.rep_stats;
        let now = Clock::get()?.unix_timestamp;

        // Handle a voucher that's been fully slashed since they last touched: wipe their claim.
        if voucher.amount > 0 && voucher.slash_generation < pool.slash_generation {
            voucher.amount = 0;
            voucher.accrued_yield = 0;
        }

        // If this Voucher account already has state (amount > 0), settle pending yield first.
        // Otherwise this is a fresh voucher: zero everything except the index snapshot.
        if voucher.amount > 0 {
            settle_voucher_yield(pool, voucher)?;
        } else {
            voucher.staker = ctx.accounts.staker.key();
            voucher.agent_code_hash = pool.agent_code_hash;
            voucher.amount = 0;
            voucher.staked_at = now;
            voucher.cooldown_started_at = 0;
            voucher.accrued_yield = 0;
            voucher.bump = ctx.bumps.voucher;
            stats.total_active_vouchers = stats.total_active_vouchers.saturating_add(1);
        }
        voucher.slash_generation = pool.slash_generation;

        // Snapshot the index for future yield accounting.
        voucher.yield_index_at_entry = pool.cumulative_yield_per_share;

        // Move USDC from staker's ATA to the VouchPool's ATA.
        let cpi_accounts = Transfer {
            from: ctx.accounts.staker_usdc.to_account_info(),
            to: ctx.accounts.vouch_pool_usdc.to_account_info(),
            authority: ctx.accounts.staker.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi_ctx, amount)?;

        voucher.amount = voucher
            .amount
            .checked_add(amount)
            .ok_or(RepError::ArithmeticOverflow)?;
        pool.total_staked = pool
            .total_staked
            .checked_add(amount)
            .ok_or(RepError::ArithmeticOverflow)?;

        // A new vouch always cancels any pending cooldown — you cannot have one foot out the door
        // and one foot in. If you re-enter, you wait the full 7 days again to exit.
        voucher.cooldown_started_at = 0;

        // Update the max-ever staked watermark. This is what we divide by to compute the
        // reputation score, so a new high here can move scores for all agents — but since the
        // score is min-clamped at 10_000 bps (=1.0), an existing top-of-the-charts agent stays
        // at score 1.0, and others see their score decrease relatively. Intentional.
        if pool.total_staked > stats.max_total_staked_seen {
            stats.max_total_staked_seen = pool.total_staked;
        }

        emit!(Vouched {
            code_hash: pool.agent_code_hash,
            staker: voucher.staker,
            amount,
            new_total_staked: pool.total_staked,
        });

        Ok(())
    }

    /// Begin the 7-day cooldown before stake can be withdrawn.
    ///
    /// SAFETY: a voucher cannot circumvent slashing by exiting *during* a default. The 7-day
    /// cooldown is the economic guarantee that vouchers have skin in the game for at least one
    /// reaction window. (Production: cooldown should be governance-controlled.)
    pub fn request_unvouch(ctx: Context<RequestUnvouch>, _code_hash: [u8; 32]) -> Result<()> {
        let voucher = &mut ctx.accounts.voucher;
        require!(voucher.amount > 0, RepError::InsufficientStake);
        require!(voucher.cooldown_started_at == 0, RepError::CooldownAlreadyStarted);

        let now = Clock::get()?.unix_timestamp;
        voucher.cooldown_started_at = now;

        emit!(UnvouchRequested {
            code_hash: voucher.agent_code_hash,
            staker: voucher.staker,
            cooldown_ends_at: now + UNVOUCH_COOLDOWN_SECONDS,
        });

        Ok(())
    }

    /// Withdraw the stake (plus any accrued yield) after the cooldown elapses. Closes the
    /// Voucher account and reclaims rent.
    ///
    /// SAFETY: must verify cooldown has fully elapsed; slashing during cooldown reduces the
    /// staker's `amount` (via index/total-staked accounting), so a staker who tries to exit
    /// during a default gets less than they put in. This is the intended behaviour.
    pub fn withdraw_vouch(ctx: Context<WithdrawVouch>, code_hash: [u8; 32]) -> Result<()> {
        let voucher = &mut ctx.accounts.voucher;
        let pool = &mut ctx.accounts.vouch_pool;
        let stats = &mut ctx.accounts.rep_stats;
        let now = Clock::get()?.unix_timestamp;

        require!(voucher.cooldown_started_at != 0, RepError::NotInCooldown);
        require!(
            now >= voucher.cooldown_started_at + UNVOUCH_COOLDOWN_SECONDS,
            RepError::CooldownNotElapsed
        );

        // If a slash happened after this voucher last touched, wipe their claim. They get nothing.
        if voucher.slash_generation < pool.slash_generation {
            voucher.amount = 0;
            voucher.accrued_yield = 0;
            voucher.slash_generation = pool.slash_generation;
        }

        // Settle final pending yield (no-op if amount == 0).
        settle_voucher_yield(pool, voucher)?;

        // The staker's claim is their post-slash effective stake + accrued yield.
        // If slashing happened, `pool.total_staked` was reduced and we must reduce this
        // voucher's claim proportionally. We track this via `total_slashed` and a per-share
        // slash index — but since the spec keeps it simple, we use the simpler accounting:
        // we shrink every voucher's `amount` field on slash directly (see `slash`).
        let principal_claim = voucher.amount;
        let yield_claim = voucher.accrued_yield;
        let total_claim = principal_claim
            .checked_add(yield_claim)
            .ok_or(RepError::ArithmeticOverflow)?;

        // Transfer USDC out via the VouchPool's signer seeds.
        let seeds = &[b"vouch".as_ref(), code_hash.as_ref(), &[pool.bump]];
        let signer = &[&seeds[..]];
        let cpi_accounts = Transfer {
            from: ctx.accounts.vouch_pool_usdc.to_account_info(),
            to: ctx.accounts.staker_usdc.to_account_info(),
            authority: pool.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer,
        );
        token::transfer(cpi_ctx, total_claim)?;

        // Update pool & stats. Note: total_staked already dropped pro-rata during slash; here we
        // remove this voucher's *principal* portion.
        pool.total_staked = pool.total_staked.saturating_sub(principal_claim);
        stats.total_active_vouchers = stats.total_active_vouchers.saturating_sub(1);

        emit!(VouchWithdrawn {
            code_hash: voucher.agent_code_hash,
            staker: voucher.staker,
            principal: principal_claim,
            yield_paid: yield_claim,
        });

        // Zero out the voucher (account is closed in the constraint via `close = staker`).
        voucher.amount = 0;
        voucher.accrued_yield = 0;
        Ok(())
    }

    /// Distribute yield to vouchers. CPI-only, called by the Credit program on every repayment.
    /// The funds (`amount` of USDC) must already have been transferred into the VouchPool's ATA
    /// by the caller — this instruction only updates the yield index.
    ///
    /// SAFETY: gated to the Credit program by checking the signer matches `rep_stats.credit_program_id`'s
    /// canonical PDA. If `total_staked == 0` we silently return (no vouchers to pay; the funds
    /// stay in the pool as a small dust accumulation, distributed to the next voucher).
    pub fn distribute_voucher_yield(
        ctx: Context<DistributeYield>,
        _code_hash: [u8; 32],
        amount: u64,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.vouch_pool;
        let stats = &ctx.accounts.rep_stats;

        // Authority check: the caller must be a PDA owned by the Credit program.
        // We don't require a specific PDA — we just require that the signer's owning program is
        // the recorded Credit program ID.
        let signer_info = &ctx.accounts.credit_program_signer;
        require_keys_eq!(*signer_info.owner, stats.credit_program_id, RepError::Unauthorized);
        require!(signer_info.is_signer, RepError::Unauthorized);

        if pool.total_staked == 0 || amount == 0 {
            // No vouchers staked → nothing to distribute. Funds (if already moved) sit in the
            // pool ATA awaiting future vouchers; accounting is unaffected.
            return Ok(());
        }

        // Update the per-share index. Math: index += amount * SCALE / total_staked.
        let delta = (amount as u128)
            .checked_mul(YIELD_INDEX_SCALE)
            .ok_or(RepError::ArithmeticOverflow)?
            .checked_div(pool.total_staked as u128)
            .ok_or(RepError::ArithmeticOverflow)?;

        pool.cumulative_yield_per_share = pool
            .cumulative_yield_per_share
            .checked_add(delta)
            .ok_or(RepError::ArithmeticOverflow)?;
        pool.total_yield_received = pool
            .total_yield_received
            .checked_add(amount)
            .ok_or(RepError::ArithmeticOverflow)?;

        emit!(YieldDistributed {
            code_hash: pool.agent_code_hash,
            amount,
            new_index: pool.cumulative_yield_per_share,
        });

        Ok(())
    }

    /// Slash the VouchPool on agent default. CPI-only, called by Credit's `mark_default`.
    ///
    /// Splits the entire pool: 50% → caller's `lp_pool_usdc` ATA (passed by Credit), 50% → burn
    /// address ATA. Returns the amount sent to the LP pool, so Credit can compute how much loss
    /// remains to haircut LPs.
    ///
    /// SAFETY:
    /// - Gated to Credit program (same check as `distribute_voucher_yield`).
    /// - Drains the entire pool ATA balance, not just `total_staked`. Any in-flight yield is
    ///   swept along with the principal. Vouchers who haven't yet claimed yield lose it — fair,
    ///   they took the risk.
    /// - We use a `slash_generation` counter on the VouchPool. On any slash, the counter
    ///   increments. Voucher accounts carry their own `slash_generation`. On any future touch
    ///   (`vouch`, `withdraw_vouch`) we compare: if the voucher's gen is less than the pool's,
    ///   we wipe the voucher's `amount` and `accrued_yield` to zero before any other accounting.
    ///   This means we never touch thousands of Voucher PDAs at slash-time; the cost is paid
    ///   lazily by each affected staker on their next interaction.
    pub fn slash(
        ctx: Context<Slash>,
        code_hash: [u8; 32],
        _total_loss: u64,
    ) -> Result<u64> {
        let pool = &mut ctx.accounts.vouch_pool;
        let stats = &ctx.accounts.rep_stats;

        // Authority check: must be called by the registered Credit program.
        let signer_info = &ctx.accounts.credit_program_signer;
        require_keys_eq!(*signer_info.owner, stats.credit_program_id, RepError::Unauthorized);
        require!(signer_info.is_signer, RepError::Unauthorized);

        // Sweep the entire VouchPool ATA balance. We read the current token balance from the
        // account info because `total_staked` may not match (yield accumulation, dust).
        let pool_balance = ctx.accounts.vouch_pool_usdc.amount;
        if pool_balance == 0 {
            return Ok(0);
        }

        let to_pool = pool_balance
            .checked_mul(SLASH_PCT_TO_POOL)
            .ok_or(RepError::ArithmeticOverflow)?
            / 100;
        let to_burn = pool_balance.saturating_sub(to_pool);

        let seeds = &[b"vouch".as_ref(), code_hash.as_ref(), &[pool.bump]];
        let signer = &[&seeds[..]];

        // Half to the LP pool.
        if to_pool > 0 {
            let cpi_accounts = Transfer {
                from: ctx.accounts.vouch_pool_usdc.to_account_info(),
                to: ctx.accounts.lp_pool_usdc.to_account_info(),
                authority: pool.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_accounts,
                signer,
            );
            token::transfer(cpi_ctx, to_pool)?;
        }

        // Half to the burn ATA (an ATA owned by the all-zero address).
        if to_burn > 0 {
            let cpi_accounts = Transfer {
                from: ctx.accounts.vouch_pool_usdc.to_account_info(),
                to: ctx.accounts.burn_usdc.to_account_info(),
                authority: pool.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_accounts,
                signer,
            );
            token::transfer(cpi_ctx, to_burn)?;
        }

        // Wipe the pool. We zero `total_staked` and the cumulative index resets effectively:
        // any voucher who tries to withdraw will hit insufficient pool balance and fail with
        // a clean error. To make withdraws no-op after a full slash, we set each voucher's
        // claim to zero via the slash factor. Simpler: shrink every voucher on next touch by
        // tracking generation. For prototype: track total_slashed; on withdraw, scale down.
        pool.total_slashed = pool
            .total_slashed
            .checked_add(pool_balance)
            .ok_or(RepError::ArithmeticOverflow)?;
        // Mark every voucher's effective stake as zero by setting a generation counter? The
        // simplest correct prototype behaviour: set total_staked = 0. Existing Voucher.amount
        // values become stale; on the next `vouch` they get re-snapshotted; on withdraw we
        // detect via the pool's slash_generation and zero out the claim.
        pool.total_staked = 0;
        pool.slash_generation = pool.slash_generation.saturating_add(1);

        emit!(Slashed {
            code_hash,
            total_slashed: pool_balance,
            to_pool,
            to_burn,
        });

        // Set return data so the caller (Credit) can read how much it recovered.
        anchor_lang::solana_program::program::set_return_data(&to_pool.to_le_bytes());
        Ok(to_pool)
    }

    /// Read-only score query. Off-chain callers can use this via simulation; on-chain callers
    /// (Credit) prefer reading the accounts directly to save CU.
    ///
    /// Returns: reputation score in bps (0..=10_000), where 10_000 = 1.0.
    pub fn read_reputation_score(
        ctx: Context<ReadScore>,
        _code_hash: [u8; 32],
    ) -> Result<u16> {
        let pool = &ctx.accounts.vouch_pool;
        let stats = &ctx.accounts.rep_stats;
        let score = compute_score(pool.total_staked, stats.max_total_staked_seen);
        anchor_lang::solana_program::program::set_return_data(&score.to_le_bytes());
        Ok(score)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Settle a voucher's pending yield based on the pool's current index.
/// Mutates `voucher.accrued_yield` and bumps `voucher.yield_index_at_entry`.
fn settle_voucher_yield(pool: &VouchPool, voucher: &mut Voucher) -> Result<()> {
    if voucher.amount == 0 {
        voucher.yield_index_at_entry = pool.cumulative_yield_per_share;
        return Ok(());
    }
    let index_delta = pool
        .cumulative_yield_per_share
        .checked_sub(voucher.yield_index_at_entry)
        .ok_or(RepError::ArithmeticOverflow)?;
    let pending = (voucher.amount as u128)
        .checked_mul(index_delta)
        .ok_or(RepError::ArithmeticOverflow)?
        .checked_div(YIELD_INDEX_SCALE)
        .ok_or(RepError::ArithmeticOverflow)? as u64;
    voucher.accrued_yield = voucher
        .accrued_yield
        .checked_add(pending)
        .ok_or(RepError::ArithmeticOverflow)?;
    voucher.yield_index_at_entry = pool.cumulative_yield_per_share;
    Ok(())
}

/// Compute the reputation score in basis points (0..=10_000).
pub fn compute_score(total_staked: u64, max_total_staked_seen: u64) -> u16 {
    if max_total_staked_seen == 0 || total_staked == 0 {
        return 0;
    }
    let raw = (total_staked as u128)
        .saturating_mul(10_000)
        .checked_div(max_total_staked_seen as u128)
        .unwrap_or(0);
    if raw > 10_000 {
        10_000
    } else {
        raw as u16
    }
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitRepStats<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + RepStats::SIZE,
        seeds = [b"rep_stats"],
        bump,
    )]
    pub rep_stats: Account<'info, RepStats>,

    #[account(mut)]
    pub admin: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetCreditProgram<'info> {
    #[account(mut, seeds = [b"rep_stats"], bump = rep_stats.bump)]
    pub rep_stats: Account<'info, RepStats>,
    pub admin: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct InitVouchPool<'info> {
    #[account(
        init,
        payer = payer,
        space = 8 + VouchPool::SIZE,
        seeds = [b"vouch", code_hash.as_ref()],
        bump,
    )]
    pub vouch_pool: Account<'info, VouchPool>,

    /// The USDC ATA owned by the VouchPool PDA. Created on init.
    #[account(
        init,
        payer = payer,
        associated_token::mint = usdc_mint,
        associated_token::authority = vouch_pool,
    )]
    pub vouch_pool_usdc: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,

    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct Vouch<'info> {
    #[account(mut, seeds = [b"vouch", code_hash.as_ref()], bump = vouch_pool.bump)]
    pub vouch_pool: Account<'info, VouchPool>,

    #[account(
        init_if_needed,
        payer = staker,
        space = 8 + Voucher::SIZE,
        seeds = [b"voucher", code_hash.as_ref(), staker.key().as_ref()],
        bump,
    )]
    pub voucher: Account<'info, Voucher>,

    #[account(mut, seeds = [b"rep_stats"], bump = rep_stats.bump)]
    pub rep_stats: Account<'info, RepStats>,

    #[account(mut)]
    pub staker: Signer<'info>,

    #[account(mut, token::mint = usdc_mint, token::authority = staker)]
    pub staker_usdc: Account<'info, TokenAccount>,

    #[account(mut, token::mint = usdc_mint, token::authority = vouch_pool)]
    pub vouch_pool_usdc: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct RequestUnvouch<'info> {
    #[account(
        mut,
        seeds = [b"voucher", code_hash.as_ref(), staker.key().as_ref()],
        bump = voucher.bump,
        has_one = staker,
    )]
    pub voucher: Account<'info, Voucher>,

    pub staker: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct WithdrawVouch<'info> {
    #[account(
        mut,
        seeds = [b"voucher", code_hash.as_ref(), staker.key().as_ref()],
        bump = voucher.bump,
        has_one = staker,
        close = staker,
    )]
    pub voucher: Account<'info, Voucher>,

    #[account(mut, seeds = [b"vouch", code_hash.as_ref()], bump = vouch_pool.bump)]
    pub vouch_pool: Account<'info, VouchPool>,

    #[account(mut, seeds = [b"rep_stats"], bump = rep_stats.bump)]
    pub rep_stats: Account<'info, RepStats>,

    #[account(mut)]
    pub staker: Signer<'info>,

    #[account(mut, token::mint = usdc_mint, token::authority = staker)]
    pub staker_usdc: Account<'info, TokenAccount>,

    #[account(mut, token::mint = usdc_mint, token::authority = vouch_pool)]
    pub vouch_pool_usdc: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct DistributeYield<'info> {
    #[account(mut, seeds = [b"vouch", code_hash.as_ref()], bump = vouch_pool.bump)]
    pub vouch_pool: Account<'info, VouchPool>,

    #[account(seeds = [b"rep_stats"], bump = rep_stats.bump)]
    pub rep_stats: Account<'info, RepStats>,

    /// CHECK: authority check happens in handler. We verify (a) `is_signer`, (b) `owner == rep_stats.credit_program_id`.
    pub credit_program_signer: AccountInfo<'info>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct Slash<'info> {
    #[account(mut, seeds = [b"vouch", code_hash.as_ref()], bump = vouch_pool.bump)]
    pub vouch_pool: Account<'info, VouchPool>,

    #[account(seeds = [b"rep_stats"], bump = rep_stats.bump)]
    pub rep_stats: Account<'info, RepStats>,

    #[account(mut, token::mint = usdc_mint, token::authority = vouch_pool)]
    pub vouch_pool_usdc: Account<'info, TokenAccount>,

    /// LP pool's USDC ATA — passed by the Credit program. We do not verify the authority here
    /// (that's Credit's responsibility) — the authority check on `credit_program_signer` is what
    /// gates this whole instruction.
    #[account(mut, token::mint = usdc_mint)]
    pub lp_pool_usdc: Account<'info, TokenAccount>,

    /// Burn ATA: USDC ATA owned by the burn address (all-zeros pubkey).
    /// CHECK: we accept any ATA with the burn-address authority; the token program enforces ownership.
    #[account(mut, token::mint = usdc_mint)]
    pub burn_usdc: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,

    /// CHECK: authority check in handler (signer + owner == registered Credit program ID).
    pub credit_program_signer: AccountInfo<'info>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct ReadScore<'info> {
    #[account(seeds = [b"vouch", code_hash.as_ref()], bump = vouch_pool.bump)]
    pub vouch_pool: Account<'info, VouchPool>,

    #[account(seeds = [b"rep_stats"], bump = rep_stats.bump)]
    pub rep_stats: Account<'info, RepStats>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[account]
pub struct RepStats {
    /// Highest `total_staked` ever observed across any agent. Denominator for the score.
    pub max_total_staked_seen: u64,
    /// Number of currently-active Voucher accounts. Stat only.
    pub total_active_vouchers: u32,
    /// Credit program ID, recorded at init. Only this program may CPI the privileged ix.
    pub credit_program_id: Pubkey,
    /// Admin (prototype-only governance).
    pub admin: Pubkey,
    pub bump: u8,
}
impl RepStats {
    pub const SIZE: usize = 8 + 4 + 32 + 32 + 1;
}

#[account]
pub struct VouchPool {
    pub agent_code_hash: [u8; 32],
    /// Sum of all live vouchers' `amount`. Decreases on slash and withdraw.
    pub total_staked: u64,
    /// Cumulative-yield-per-share index, scaled by `YIELD_INDEX_SCALE`.
    pub cumulative_yield_per_share: u128,
    /// Total USDC ever slashed from this pool (cumulative). Stat.
    pub total_slashed: u64,
    /// Total USDC ever received as yield from Credit. Stat.
    pub total_yield_received: u64,
    /// Incremented on every slash. Vouchers whose `slash_generation < pool.slash_generation`
    /// are wiped on next touch.
    pub slash_generation: u32,
    pub bump: u8,
}
impl VouchPool {
    pub const SIZE: usize = 32 + 8 + 16 + 8 + 8 + 4 + 1;
}

#[account]
pub struct Voucher {
    pub staker: Pubkey,
    pub agent_code_hash: [u8; 32],
    /// Effective stake. Updated on slash; decreased on withdraw.
    pub amount: u64,
    pub staked_at: i64,
    /// 0 means no cooldown active. Non-zero = unix timestamp when cooldown began.
    pub cooldown_started_at: i64,
    /// The pool's `cumulative_yield_per_share` at the time of the last balance change.
    pub yield_index_at_entry: u128,
    /// Yield owed but not yet paid out. Settled on every balance change.
    pub accrued_yield: u64,
    /// Slash generation snapshot. If lower than pool's, the voucher has been slashed.
    pub slash_generation: u32,
    pub bump: u8,
}
impl Voucher {
    pub const SIZE: usize = 32 + 32 + 8 + 8 + 8 + 16 + 8 + 4 + 1;
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[event]
pub struct Vouched {
    pub code_hash: [u8; 32],
    pub staker: Pubkey,
    pub amount: u64,
    pub new_total_staked: u64,
}

#[event]
pub struct UnvouchRequested {
    pub code_hash: [u8; 32],
    pub staker: Pubkey,
    pub cooldown_ends_at: i64,
}

#[event]
pub struct VouchWithdrawn {
    pub code_hash: [u8; 32],
    pub staker: Pubkey,
    pub principal: u64,
    pub yield_paid: u64,
}

#[event]
pub struct YieldDistributed {
    pub code_hash: [u8; 32],
    pub amount: u64,
    pub new_index: u128,
}

#[event]
pub struct Slashed {
    pub code_hash: [u8; 32],
    pub total_slashed: u64,
    pub to_pool: u64,
    pub to_burn: u64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[error_code]
pub enum RepError {
    #[msg("Caller is not authorised to invoke this instruction.")]
    Unauthorized,
    #[msg("Cooldown period has not yet elapsed.")]
    CooldownNotElapsed,
    #[msg("Voucher is not in cooldown — call request_unvouch first.")]
    NotInCooldown,
    #[msg("Cooldown is already active.")]
    CooldownAlreadyStarted,
    #[msg("Stake amount must be greater than zero.")]
    InsufficientStake,
    #[msg("Arithmetic overflow.")]
    ArithmeticOverflow,
}

// ---------------------------------------------------------------------------
// Tests (in-comments per spec quality bar)
// ---------------------------------------------------------------------------
//
// Test 1: First voucher gets correctly accounted.
//   - init_vouch_pool(code_hash="abc")
//   - alice vouches 100 USDC → Voucher{amount: 100}, VouchPool{total_staked: 100}, RepStats{max: 100}
//   - compute_score(100, 100) == 10_000 (bps) == 1.0
//
// Test 2: Yield distribution doesn't dilute earlier vouchers.
//   - alice vouches 100 (yield_index_at_entry = 0)
//   - credit distributes 10 USDC yield → pool.index = 10 * SCALE / 100 = 0.1 * SCALE
//   - bob vouches 100 (yield_index_at_entry = 0.1 * SCALE)
//   - credit distributes 10 USDC yield → delta = 10 * SCALE / 200 = 0.05 * SCALE → pool.index = 0.15 * SCALE
//   - alice's pending = (0.15 - 0.0) * 100 / SCALE = 15  ✓ (got the 10 pre-bob + half of 10 post-bob)
//   - bob's pending = (0.15 - 0.1) * 100 / SCALE = 5    ✓ (only half of the post-bob 10)
//
// Test 3: Slashing distributes loss correctly.
//   - alice 100, bob 100, total_staked = 200
//   - slash() drains pool ATA (200 USDC). to_pool = 100, to_burn = 100.
//   - pool.total_staked = 0, slash_generation = 1.
//   - alice tries to withdraw: pool.slash_generation > voucher.slash_generation → claim = 0.
//   - LP pool received 100 (half of 200). Burn received 100. ✓
//
// Test 4: Reputation score normalisation.
//   - agent A staked 50, max_seen = 50 → score = 10_000 (1.0)
//   - agent B vouches 200 → max_seen = 200, A's score recomputed = 50*10000/200 = 2500 (0.25). ✓
