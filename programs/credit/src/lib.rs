//! polis Credit primitive — single-pool USDC lending against reputation.
//!
//! Per spec §Credit. One LP pool funds all agents. Borrows are sized and priced by an agent's
//! reputation score (read from the Reputation program's `VouchPool` and `RepStats` PDAs).
//! Defaults flow through Reputation slashing → 50% recovered, 50% burned; any remaining loss
//! is haircut pro-rata across LP shares by reducing `total_deposits` without changing
//! `total_shares`.
//!
//! ## Share accounting
//!
//! Standard pool-share design (cf. Compound's cToken):
//!   - First deposit: shares = amount (1:1).
//!   - Subsequent deposits: shares = amount * total_shares / total_deposits.
//!   - Withdraw: usdc = shares * total_deposits / total_shares.
//!   - Yield from repaid interest (the 70% slice kept by the pool) is added to `total_deposits`
//!     without minting new shares, so per-share value grows.
//!   - Loss from default (post-slash haircut) subtracts from `total_deposits` similarly.
//!
//! ## Reputation coupling
//!
//! Credit holds the Reputation program's ID via the `reputation` crate dependency. On `borrow`
//! it reads `VouchPool` and `RepStats` accounts directly (no CPI needed — we just deserialise
//! them), computes the score, and looks up the credit limit / APR from the hardcoded prototype
//! table. On `repay` and `mark_default` it CPIs into Reputation.
//!
//! ## Pool signer pattern for CPI authority
//!
//! Reputation's privileged ix (`distribute_voucher_yield`, `slash`) check that the signer's
//! `owner` field equals the registered Credit program ID. We satisfy this by having the
//! Pool PDA itself sign the CPI — `Pool` is owned by the Credit program, so Reputation accepts.
//! No separate "credit_signer" PDA needed.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::pubkey::Pubkey;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use reputation::cpi::accounts::{DistributeYield, Slash};
use reputation::program::Reputation;
use reputation::{compute_score, RepStats, VouchPool};

declare_id!("7jCmeDoXpMFRTsWYuCeKvx1TiHF9pjT4iyTKLQaPaHDQ");

/// Hard-bound the wallet program ID at compile time. Closes the gap where
/// `borrow` would accept any pubkey as `agent_wallet_pda` and just record it
/// on the CreditLine. We now derive the canonical wallet PDA from
/// `("wallet", code_hash)` against this constant and require equality.
pub const WALLET_PROGRAM_ID: Pubkey =
    anchor_lang::pubkey!("7Jz35XjSeWfA28t8oxzbTyfUTuvgiM4YYnkygpktALBY");

/// Default grace period before a credit line can be marked defaulted. 30 days per spec.
pub const DEFAULT_DEFAULT_AFTER_SECONDS: i64 = 30 * 24 * 60 * 60;

/// Seconds in a (non-leap) year, used as the APR denominator.
pub const SECONDS_PER_YEAR: u128 = 31_536_000;

/// 100% in basis points.
pub const BPS_DENOM: u128 = 10_000;

/// Share of paid interest that flows to vouchers as yield (the rest is kept by the pool).
/// 30% per spec §Reputation.
pub const VOUCHER_YIELD_PCT: u64 = 30;
pub const POOL_KEEP_PCT: u64 = 70;

/// Credit-limit / APR table per spec §Reputation. Prototype-hardcoded; production governed.
/// Input: reputation score in bps (0..=10_000). Output: (max_borrow_in_USDC_base_units, apr_bps).
/// 1 USDC = 1_000_000 base units (USDC has 6 decimals).
pub fn credit_limit_and_apr(reputation_score_bps: u16) -> (u64, u16) {
    // Spec table in score (ratio) form maps to bps as follows:
    //   <0.01      => 0..99
    //   0.01..0.05 => 100..499
    //   0.05..0.20 => 500..1999
    //   0.20..0.50 => 2000..4999
    //   >=0.50     => 5000..=10000
    match reputation_score_bps {
        0..=99       => (0, 0),
        100..=499    => (500_000_000, 3650),        // L1: $500, 36.5% APR
        500..=1999   => (5_000_000_000, 2400),      // L2: $5K, 24%
        2000..=4999  => (50_000_000_000, 1800),     // L3: $50K, 18%
        _            => (500_000_000_000, 1200),    // L4: $500K, 12%
    }
}

#[program]
pub mod credit {
    use super::*;

    /// One-time pool initialisation. Creates the Pool PDA and its USDC ATA. Admin-only.
    ///
    /// SAFETY: only the admin can call this. Sets the `reputation_program_id` field so we know
    /// where to CPI to. The Pool PDA is the authority on the pool USDC ATA, and is also the
    /// signer used to satisfy Reputation's "caller must be credit program" check.
    pub fn init_pool(
        ctx: Context<InitPool>,
        default_after_seconds: i64,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.admin = ctx.accounts.admin.key();
        pool.usdc_mint = ctx.accounts.usdc_mint.key();
        pool.usdc_vault = ctx.accounts.pool_usdc.key();
        pool.reputation_program = ctx.accounts.reputation_program.key();
        pool.total_deposits = 0;
        pool.total_shares = 0;
        pool.total_drawn = 0;
        pool.total_interest_accrued = 0;
        pool.total_interest_repaid = 0;
        pool.total_principal_repaid = 0;
        pool.total_defaulted_principal = 0;
        pool.total_lp_haircut = 0;
        pool.default_after_seconds = if default_after_seconds > 0 {
            default_after_seconds
        } else {
            DEFAULT_DEFAULT_AFTER_SECONDS
        };
        pool.bump = ctx.bumps.pool;
        Ok(())
    }

    /// LP deposit. Mints shares pro-rata to current per-share value.
    ///
    /// SAFETY: invariant preserved — `total_shares / total_deposits` is the per-share value;
    /// new depositors get shares such that `value * new_shares = amount`. First deposit is 1:1.
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, CreditError::ZeroAmount);

        let pool = &mut ctx.accounts.pool;
        let deposit_acc = &mut ctx.accounts.deposit;
        let now = Clock::get()?.unix_timestamp;

        // shares = amount * total_shares / total_deposits (or amount on first deposit)
        let new_shares: u64 = if pool.total_shares == 0 || pool.total_deposits == 0 {
            amount
        } else {
            (amount as u128)
                .checked_mul(pool.total_shares as u128)
                .ok_or(CreditError::ArithmeticOverflow)?
                .checked_div(pool.total_deposits as u128)
                .ok_or(CreditError::ArithmeticOverflow)? as u64
        };
        require!(new_shares > 0, CreditError::ZeroShares);

        // Transfer USDC in.
        let cpi_accounts = Transfer {
            from: ctx.accounts.lp_usdc.to_account_info(),
            to: ctx.accounts.pool_usdc.to_account_info(),
            authority: ctx.accounts.lp.to_account_info(),
        };
        token::transfer(
            CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts),
            amount,
        )?;

        // Update pool & deposit. init_if_needed handles first-time depositors.
        if deposit_acc.lp == Pubkey::default() {
            deposit_acc.lp = ctx.accounts.lp.key();
            deposit_acc.deposited_at = now;
            deposit_acc.bump = ctx.bumps.deposit;
        }
        deposit_acc.shares = deposit_acc
            .shares
            .checked_add(new_shares)
            .ok_or(CreditError::ArithmeticOverflow)?;

        pool.total_deposits = pool
            .total_deposits
            .checked_add(amount)
            .ok_or(CreditError::ArithmeticOverflow)?;
        pool.total_shares = pool
            .total_shares
            .checked_add(new_shares)
            .ok_or(CreditError::ArithmeticOverflow)?;

        emit!(Deposited {
            lp: deposit_acc.lp,
            amount,
            shares_minted: new_shares,
            new_total_deposits: pool.total_deposits,
            new_total_shares: pool.total_shares,
        });

        Ok(())
    }

    /// LP withdrawal. Burns shares and returns the proportional USDC amount.
    ///
    /// SAFETY: requires that withdrawn USDC ≤ `available = total_deposits - total_drawn`,
    /// i.e. we cannot withdraw money that's currently lent out. The borrowed money must come
    /// back via `repay` before LPs can fully exit.
    pub fn withdraw(ctx: Context<Withdraw>, shares: u64) -> Result<()> {
        require!(shares > 0, CreditError::ZeroShares);

        let pool = &mut ctx.accounts.pool;
        let deposit_acc = &mut ctx.accounts.deposit;

        require!(
            deposit_acc.shares >= shares,
            CreditError::InsufficientShares
        );
        require!(pool.total_shares > 0, CreditError::PoolEmpty);

        let amount: u64 = (shares as u128)
            .checked_mul(pool.total_deposits as u128)
            .ok_or(CreditError::ArithmeticOverflow)?
            .checked_div(pool.total_shares as u128)
            .ok_or(CreditError::ArithmeticOverflow)? as u64;

        // Cannot withdraw money currently lent out.
        let available = pool
            .total_deposits
            .checked_sub(pool.total_drawn)
            .ok_or(CreditError::ArithmeticOverflow)?;
        require!(amount <= available, CreditError::InsufficientLiquidity);

        // Pool signs as the ATA authority.
        let bump = pool.bump;
        let seeds = &[b"pool".as_ref(), &[bump]];
        let signer = &[&seeds[..]];
        let cpi_accounts = Transfer {
            from: ctx.accounts.pool_usdc.to_account_info(),
            to: ctx.accounts.lp_usdc.to_account_info(),
            authority: pool.to_account_info(),
        };
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_accounts,
                signer,
            ),
            amount,
        )?;

        deposit_acc.shares -= shares;
        pool.total_shares -= shares;
        pool.total_deposits = pool
            .total_deposits
            .checked_sub(amount)
            .ok_or(CreditError::ArithmeticOverflow)?;

        emit!(Withdrawn {
            lp: deposit_acc.lp,
            shares_burned: shares,
            amount_returned: amount,
            new_total_deposits: pool.total_deposits,
            new_total_shares: pool.total_shares,
        });

        Ok(())
    }

    /// Agent borrow. Opens or extends a credit line. The amount and APR are determined by the
    /// agent's reputation score (read directly from Reputation PDAs).
    ///
    /// SAFETY:
    /// - We re-derive the reputation PDAs from `code_hash` and verify the addresses match the
    ///   passed accounts. This prevents an attacker from passing a different agent's VouchPool
    ///   to inflate the score.
    /// - Verify `agent_wallet_pda` is the canonical wallet for this code_hash by checking
    ///   `seeds=("wallet", code_hash)` and the recorded wallet program ID. We require the caller
    ///   to pass the wallet's USDC ATA; the agent's wallet program is responsible for vetting
    ///   inflows on its side, but we record `agent_wallet_pda` on the CreditLine so future
    ///   `repay` and `mark_default` can verify it.
    /// - Interest is accrued *before* extending the line, so the APR change applies only to
    ///   future periods.
    pub fn borrow(
        ctx: Context<Borrow>,
        code_hash: [u8; 32],
        amount: u64,
        agent_wallet_pda: Pubkey,
    ) -> Result<()> {
        require!(amount > 0, CreditError::ZeroAmount);

        // 1. Verify the passed VouchPool PDA is canonical for `code_hash`.
        let (expected_vouch, _b) = Pubkey::find_program_address(
            &[b"vouch", code_hash.as_ref()],
            &ctx.accounts.pool.reputation_program,
        );
        require_keys_eq!(
            ctx.accounts.vouch_pool.key(),
            expected_vouch,
            CreditError::InvalidReputationAccount
        );
        // RepStats PDA derived from "rep_stats" — single global.
        let (expected_stats, _b) =
            Pubkey::find_program_address(&[b"rep_stats"], &ctx.accounts.pool.reputation_program);
        require_keys_eq!(
            ctx.accounts.rep_stats.key(),
            expected_stats,
            CreditError::InvalidReputationAccount
        );

        // 2. Verify `agent_wallet_pda` is the canonical ("wallet", code_hash) PDA owned by
        //    the wallet program. Closes the gap where the caller could supply an arbitrary
        //    pubkey and have it recorded as the agent's wallet.
        let (expected_wallet_pda, _wallet_bump) = Pubkey::find_program_address(
            &[b"wallet", code_hash.as_ref()],
            &WALLET_PROGRAM_ID,
        );
        require_keys_eq!(
            agent_wallet_pda,
            expected_wallet_pda,
            CreditError::WalletMismatch
        );

        // 3. Verify the agent wallet's USDC ATA is actually owned by `agent_wallet_pda`. Without
        //    this check, the caller could specify any USDC ATA and divert the borrowed funds.
        require_keys_eq!(
            ctx.accounts.agent_wallet_usdc.owner,
            agent_wallet_pda,
            CreditError::WalletMismatch
        );

        // 3. Compute the score and look up the credit envelope.
        let score = compute_score(
            ctx.accounts.vouch_pool.total_staked,
            ctx.accounts.rep_stats.max_total_staked_seen,
        );
        let (limit, apr_bps) = credit_limit_and_apr(score);
        require!(limit > 0, CreditError::InsufficientReputation);

        // 3. Accrue interest on any existing principal before mutating it.
        let now = Clock::get()?.unix_timestamp;
        let line = &mut ctx.accounts.credit_line;

        if line.is_active {
            require_keys_eq!(
                line.agent_wallet_pda,
                agent_wallet_pda,
                CreditError::WalletMismatch
            );
            accrue_interest(line, now)?;
        } else {
            // Fresh credit line. Note: if this is a re-borrow on a previously-closed line, we
            // also reset the timestamps (treating it as a new lending event).
            line.agent_code_hash = code_hash;
            line.agent_wallet_pda = agent_wallet_pda;
            line.principal = 0;
            line.accrued_interest = 0;
            line.opened_at = now;
            line.last_accrual = now;
            line.last_repay_at = now; // a fresh draw resets the default clock
            line.is_active = true;
            line.bump = ctx.bumps.credit_line;
        }

        // 4. Enforce the credit limit on TOTAL outstanding (principal + interest + new draw).
        let total_after = (line.principal as u128)
            .checked_add(line.accrued_interest as u128)
            .ok_or(CreditError::ArithmeticOverflow)?
            .checked_add(amount as u128)
            .ok_or(CreditError::ArithmeticOverflow)?;
        require!(
            total_after <= limit as u128,
            CreditError::AmountExceedsLimit
        );

        // 5. Enforce pool liquidity.
        let pool = &mut ctx.accounts.pool;
        let available = pool
            .total_deposits
            .checked_sub(pool.total_drawn)
            .ok_or(CreditError::ArithmeticOverflow)?;
        require!(amount <= available, CreditError::InsufficientLiquidity);

        // 6. Update credit line state. APR is re-quoted on every fresh draw (uses the current
        //    score). Existing accrued interest already used the OLD apr — that's fine, we accrued
        //    in step 3 before changing the rate.
        line.principal = line
            .principal
            .checked_add(amount)
            .ok_or(CreditError::ArithmeticOverflow)?;
        line.apr_bps = apr_bps;
        line.last_accrual = now;

        // 7. Transfer USDC from Pool ATA to the agent wallet's USDC ATA.
        let bump = pool.bump;
        let seeds = &[b"pool".as_ref(), &[bump]];
        let signer = &[&seeds[..]];
        let cpi_accounts = Transfer {
            from: ctx.accounts.pool_usdc.to_account_info(),
            to: ctx.accounts.agent_wallet_usdc.to_account_info(),
            authority: pool.to_account_info(),
        };
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_accounts,
                signer,
            ),
            amount,
        )?;

        pool.total_drawn = pool
            .total_drawn
            .checked_add(amount)
            .ok_or(CreditError::ArithmeticOverflow)?;

        emit!(Borrowed {
            code_hash,
            amount,
            new_principal: line.principal,
            apr_bps,
            score_bps: score,
        });

        Ok(())
    }

    /// Public interest-accrual ticker. Anyone can call. Idempotent.
    ///
    /// SAFETY: pure update of `accrued_interest` and `last_accrual`. No fund movement.
    pub fn accrue(ctx: Context<Accrue>, _code_hash: [u8; 32]) -> Result<()> {
        let line = &mut ctx.accounts.credit_line;
        let now = Clock::get()?.unix_timestamp;
        accrue_interest(line, now)?;
        Ok(())
    }

    /// Repay an open credit line. Interest is paid first (with 30% of the interest slice routed
    /// to the agent's VouchPool as yield via CPI to Reputation), then principal.
    ///
    /// SAFETY:
    /// - Agent (or any signer with USDC) pays; we don't verify who pays. Anyone can repay an
    ///   agent's debt. This is fine — only helps the agent. The signer's USDC ATA is debited.
    /// - 70% of interest stays in the pool and grows `total_deposits` (yield to LPs).
    /// - 30% of interest is transferred to the VouchPool's USDC ATA, then we CPI
    ///   `distribute_voucher_yield` so Reputation updates its yield index.
    /// - Principal payments reduce both `total_drawn` and `line.principal`.
    pub fn repay(ctx: Context<Repay>, code_hash: [u8; 32], amount: u64) -> Result<()> {
        require!(amount > 0, CreditError::ZeroAmount);

        let now = Clock::get()?.unix_timestamp;
        let line = &mut ctx.accounts.credit_line;
        require!(line.is_active, CreditError::NoActiveLine);
        require!(line.agent_code_hash == code_hash, CreditError::WalletMismatch);

        // Verify the VouchPool PDA passed for the yield routing matches code_hash.
        let (expected_vouch, _b) = Pubkey::find_program_address(
            &[b"vouch", code_hash.as_ref()],
            &ctx.accounts.pool.reputation_program,
        );
        require_keys_eq!(
            ctx.accounts.vouch_pool.key(),
            expected_vouch,
            CreditError::InvalidReputationAccount
        );

        // Accrue interest up to now before computing payment split.
        accrue_interest(line, now)?;

        // Pay interest first.
        let mut remaining = amount;
        let interest_payment = remaining.min(line.accrued_interest);
        line.accrued_interest -= interest_payment;
        remaining -= interest_payment;

        // Split interest into pool-keep (70%) and voucher-yield (30%).
        let voucher_share = interest_payment
            .checked_mul(VOUCHER_YIELD_PCT)
            .ok_or(CreditError::ArithmeticOverflow)?
            / 100;
        let pool_share = interest_payment.saturating_sub(voucher_share);

        // Principal payment with whatever's left.
        let principal_payment = remaining.min(line.principal);
        line.principal -= principal_payment;
        remaining -= principal_payment;
        // If `remaining > 0` the agent overpaid; we refuse rather than silently swallow it.
        require!(remaining == 0, CreditError::OverPayment);

        // Move funds:
        //   (a) interest + principal flowing to Pool: pool_share + principal_payment
        //   (b) voucher share flowing to VouchPool ATA: voucher_share
        // Total = interest_payment + principal_payment = amount.
        let pool_inflow = pool_share
            .checked_add(principal_payment)
            .ok_or(CreditError::ArithmeticOverflow)?;

        if pool_inflow > 0 {
            let cpi_accounts = Transfer {
                from: ctx.accounts.payer_usdc.to_account_info(),
                to: ctx.accounts.pool_usdc.to_account_info(),
                authority: ctx.accounts.payer.to_account_info(),
            };
            token::transfer(
                CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts),
                pool_inflow,
            )?;
        }

        if voucher_share > 0 {
            let cpi_accounts = Transfer {
                from: ctx.accounts.payer_usdc.to_account_info(),
                to: ctx.accounts.vouch_pool_usdc.to_account_info(),
                authority: ctx.accounts.payer.to_account_info(),
            };
            token::transfer(
                CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts),
                voucher_share,
            )?;

            // Inform Reputation to update its yield index. The Pool PDA signs (because it is
            // owned by the Credit program — that's the authority Reputation checks).
            let pool_bump = ctx.accounts.pool.bump;
            let pool_seeds = &[b"pool".as_ref(), &[pool_bump]];
            let signer = &[&pool_seeds[..]];
            let cpi_program = ctx.accounts.reputation_program.to_account_info();
            let cpi_accounts = DistributeYield {
                vouch_pool: ctx.accounts.vouch_pool.to_account_info(),
                rep_stats: ctx.accounts.rep_stats.to_account_info(),
                credit_program_signer: ctx.accounts.pool.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
            reputation::cpi::distribute_voucher_yield(cpi_ctx, code_hash, voucher_share)?;
        }

        // Update pool accounting.
        let pool = &mut ctx.accounts.pool;
        // Yield (pool_share of interest) increases total_deposits → per-share value grows.
        pool.total_deposits = pool
            .total_deposits
            .checked_add(pool_share)
            .ok_or(CreditError::ArithmeticOverflow)?;
        pool.total_drawn = pool.total_drawn.saturating_sub(principal_payment);
        pool.total_interest_repaid = pool
            .total_interest_repaid
            .checked_add(interest_payment)
            .ok_or(CreditError::ArithmeticOverflow)?;
        pool.total_principal_repaid = pool
            .total_principal_repaid
            .checked_add(principal_payment)
            .ok_or(CreditError::ArithmeticOverflow)?;

        // If the line is fully paid, mark inactive but retain history.
        if line.principal == 0 && line.accrued_interest == 0 {
            line.is_active = false;
        }
        line.last_accrual = now;
        line.last_repay_at = now; // any repayment resets the default clock

        emit!(Repaid {
            code_hash,
            interest_paid: interest_payment,
            principal_paid: principal_payment,
            voucher_yield: voucher_share,
            line_closed: !line.is_active,
        });

        Ok(())
    }

    /// Mark a defaulted line. Anyone can call. Triggers slashing CPI and an LP haircut.
    ///
    /// SAFETY:
    /// - Eligibility: `now - line.last_accrual > pool.default_after_seconds` AND the line is
    ///   active with non-zero outstanding.
    /// - CPI into Reputation::slash. Reputation reads the current pool ATA balance and sweeps
    ///   it 50/50 (pool / burn). The recovered amount is returned via `set_return_data`.
    /// - Any remaining loss (outstanding - recovered) is haircut from `total_deposits` →
    ///   pro-rata across all LP shares (their per-share value drops, no shares burned).
    /// - The line is closed: `is_active = false`, principal & interest zeroed (the loss is
    ///   booked).
    pub fn mark_default<'info>(
        ctx: Context<'_, '_, '_, 'info, MarkDefault<'info>>,
        code_hash: [u8; 32],
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let line = &mut ctx.accounts.credit_line;
        require!(line.is_active, CreditError::NoActiveLine);
        require!(line.agent_code_hash == code_hash, CreditError::WalletMismatch);

        // Verify the VouchPool PDA is canonical for this code_hash.
        let (expected_vouch, _b) = Pubkey::find_program_address(
            &[b"vouch", code_hash.as_ref()],
            &ctx.accounts.pool.reputation_program,
        );
        require_keys_eq!(
            ctx.accounts.vouch_pool.key(),
            expected_vouch,
            CreditError::InvalidReputationAccount
        );

        let pool = &mut ctx.accounts.pool;

        // Eligibility: agent has held an open balance for longer than `default_after_seconds`
        // without any repayment. `last_repay_at` is updated by `borrow` (fresh draw resets the
        // clock) and by `repay` (any payment resets the clock). If neither has happened in the
        // grace period, the line defaults.
        require!(
            now.saturating_sub(line.last_repay_at) >= pool.default_after_seconds,
            CreditError::NotDefaulted
        );

        // Re-accrue so the outstanding number is current (after the eligibility check, so the
        // accrual doesn't itself satisfy the time requirement).
        accrue_interest(line, now)?;

        let outstanding = line
            .principal
            .checked_add(line.accrued_interest)
            .ok_or(CreditError::ArithmeticOverflow)?;
        require!(outstanding > 0, CreditError::NoActiveLine);

        // Verify the burn ATA is owned by the canonical burn address (all-zeros pubkey).
        // Without this check, a malicious caller could direct the "burned" half to an attacker
        // address (still 50% goes to Pool, but the burn portion is meant to be unrecoverable).
        require_keys_eq!(
            ctx.accounts.burn_usdc.owner,
            reputation::burn_address(),
            CreditError::InvalidReputationAccount
        );

        // CPI into Reputation::slash. The Pool PDA signs (it is owned by the Credit program,
        // which satisfies Reputation's authority check).
        let pool_bump = pool.bump;
        let pool_seeds = &[b"pool".as_ref(), &[pool_bump]];
        let signer = &[&pool_seeds[..]];

        let cpi_program = ctx.accounts.reputation_program.to_account_info();
        let cpi_accounts = Slash {
            vouch_pool: ctx.accounts.vouch_pool.to_account_info(),
            rep_stats: ctx.accounts.rep_stats.to_account_info(),
            vouch_pool_usdc: ctx.accounts.vouch_pool_usdc.to_account_info(),
            lp_pool_usdc: ctx.accounts.pool_usdc.to_account_info(),
            burn_usdc: ctx.accounts.burn_usdc.to_account_info(),
            usdc_mint: ctx.accounts.usdc_mint.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            credit_program_signer: pool.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
        reputation::cpi::slash(cpi_ctx, code_hash, outstanding)?;

        // Read the recovered amount from return data.
        let (_program_id, return_data) =
            anchor_lang::solana_program::program::get_return_data().unwrap_or((Pubkey::default(), vec![]));
        let recovered: u64 = if return_data.len() >= 8 {
            u64::from_le_bytes(return_data[..8].try_into().unwrap())
        } else {
            0
        };

        // Compute remaining loss after recovery.
        // The pool's `total_drawn` is the principal currently lent out; once a default is booked
        // we must reduce `total_drawn` (the principal isn't coming back) and reduce
        // `total_deposits` by any uncovered loss.
        let principal_lost = line.principal;
        let interest_unpaid = line.accrued_interest; // booked-but-uncollected interest; doesn't hit deposits

        // Loss to LPs: only the principal that isn't recovered. Interest that was accrued but
        // not paid is "lost revenue" — it doesn't reduce deposits because we never added it.
        let lp_loss = principal_lost.saturating_sub(recovered);

        pool.total_drawn = pool.total_drawn.saturating_sub(principal_lost);
        // Recovered USDC has already physically arrived in the pool ATA via the slash CPI.
        // We treat the recovery as principal repayment (no new shares issued, deposits unchanged
        // if recovered == principal_lost; otherwise we add the recovered amount back to deposits
        // and reduce by the full principal_lost: net change = -lp_loss).
        // Implementation: add recovered, subtract principal_lost.
        pool.total_deposits = pool
            .total_deposits
            .saturating_add(recovered)
            .saturating_sub(principal_lost);
        pool.total_defaulted_principal = pool
            .total_defaulted_principal
            .checked_add(principal_lost)
            .ok_or(CreditError::ArithmeticOverflow)?;
        pool.total_lp_haircut = pool
            .total_lp_haircut
            .checked_add(lp_loss)
            .ok_or(CreditError::ArithmeticOverflow)?;

        // Close the line — book the loss.
        line.is_active = false;
        line.principal = 0;
        line.accrued_interest = 0;

        emit!(Defaulted {
            code_hash,
            principal_lost,
            interest_unpaid,
            recovered_from_vouchers: recovered,
            lp_haircut: lp_loss,
        });

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Accrue interest on a credit line from `last_accrual` up to `now`. Uses u128 intermediates.
fn accrue_interest(line: &mut CreditLine, now: i64) -> Result<()> {
    if !line.is_active {
        return Ok(());
    }
    if now <= line.last_accrual {
        return Ok(()); // clock skew or same-block; no-op
    }
    let dt = (now - line.last_accrual) as u128;
    if line.principal == 0 || line.apr_bps == 0 {
        line.last_accrual = now;
        return Ok(());
    }
    // ΔI = principal * apr_bps * Δt / (10_000 * 31_536_000)
    let numerator = (line.principal as u128)
        .checked_mul(line.apr_bps as u128)
        .ok_or(CreditError::AccrualOverflow)?
        .checked_mul(dt)
        .ok_or(CreditError::AccrualOverflow)?;
    let denom = BPS_DENOM
        .checked_mul(SECONDS_PER_YEAR)
        .ok_or(CreditError::AccrualOverflow)?;
    let delta = numerator
        .checked_div(denom)
        .ok_or(CreditError::AccrualOverflow)?;
    let delta_u64: u64 = delta
        .try_into()
        .map_err(|_| CreditError::AccrualOverflow)?;
    line.accrued_interest = line
        .accrued_interest
        .checked_add(delta_u64)
        .ok_or(CreditError::AccrualOverflow)?;
    line.last_accrual = now;
    Ok(())
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitPool<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + Pool::SIZE,
        seeds = [b"pool"],
        bump,
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        init,
        payer = admin,
        associated_token::mint = usdc_mint,
        associated_token::authority = pool,
    )]
    pub pool_usdc: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,

    /// Reputation program — recorded on the pool so we know where to CPI later.
    /// CHECK: just the program account; validated by Anchor only as an executable.
    pub reputation_program: Program<'info, Reputation>,

    #[account(mut)]
    pub admin: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut, seeds = [b"pool"], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(
        init_if_needed,
        payer = lp,
        space = 8 + DepositRec::SIZE,
        seeds = [b"deposit", lp.key().as_ref()],
        bump,
    )]
    pub deposit: Account<'info, DepositRec>,

    #[account(mut)]
    pub lp: Signer<'info>,

    #[account(mut, token::mint = usdc_mint, token::authority = lp)]
    pub lp_usdc: Account<'info, TokenAccount>,

    #[account(mut, token::mint = usdc_mint, token::authority = pool)]
    pub pool_usdc: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut, seeds = [b"pool"], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"deposit", lp.key().as_ref()],
        bump = deposit.bump,
        has_one = lp,
    )]
    pub deposit: Account<'info, DepositRec>,

    #[account(mut)]
    pub lp: Signer<'info>,

    #[account(mut, token::mint = usdc_mint, token::authority = lp)]
    pub lp_usdc: Account<'info, TokenAccount>,

    #[account(mut, token::mint = usdc_mint, token::authority = pool)]
    pub pool_usdc: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct Borrow<'info> {
    #[account(mut, seeds = [b"pool"], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(
        init_if_needed,
        payer = caller,
        space = 8 + CreditLine::SIZE,
        seeds = [b"credit", code_hash.as_ref()],
        bump,
    )]
    pub credit_line: Account<'info, CreditLine>,

    /// Reputation VouchPool for this agent. Account-address verified in handler.
    pub vouch_pool: Account<'info, VouchPool>,

    /// Reputation global stats. Account-address verified in handler.
    pub rep_stats: Account<'info, RepStats>,

    /// Pool USDC vault (authority: Pool PDA).
    /// Boxed (with agent_wallet_usdc) to keep `try_accounts` under the 4 KB SBF stack frame.
    #[account(mut, token::mint = usdc_mint, token::authority = pool)]
    pub pool_usdc: Box<Account<'info, TokenAccount>>,

    /// Agent's wallet USDC ATA — proceeds land here. The wallet program is responsible for any
    /// inbound validation; we just record the wallet PDA on the credit line.
    #[account(mut, token::mint = usdc_mint)]
    pub agent_wallet_usdc: Box<Account<'info, TokenAccount>>,

    /// The signer who actually invokes borrow. Pays rent for credit_line account if first time.
    /// This is typically the agent itself (signing via the wallet program's PDA cross-sign in
    /// production; for prototype, any signer can initiate — the funds always flow to the
    /// agent_wallet_usdc account, not to the caller).
    #[account(mut)]
    pub caller: Signer<'info>,

    pub usdc_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct Accrue<'info> {
    #[account(
        mut,
        seeds = [b"credit", code_hash.as_ref()],
        bump = credit_line.bump,
    )]
    pub credit_line: Account<'info, CreditLine>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct Repay<'info> {
    #[account(mut, seeds = [b"pool"], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"credit", code_hash.as_ref()],
        bump = credit_line.bump,
    )]
    pub credit_line: Account<'info, CreditLine>,

    /// VouchPool to route the 30% interest slice to. Address verified in handler.
    /// CHECK: typed as AccountInfo so we can CPI into Reputation with it without re-deserialising.
    #[account(mut)]
    pub vouch_pool: Account<'info, VouchPool>,

    /// VouchPool's USDC ATA. The 30% interest slice is transferred here.
    #[account(mut, token::mint = usdc_mint, token::authority = vouch_pool)]
    pub vouch_pool_usdc: Account<'info, TokenAccount>,

    /// Reputation global stats — passed through for the CPI to read.
    pub rep_stats: Account<'info, RepStats>,

    /// Whoever is repaying — usually the agent, but anyone can repay an agent's debt.
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(mut, token::mint = usdc_mint, token::authority = payer)]
    pub payer_usdc: Account<'info, TokenAccount>,

    #[account(mut, token::mint = usdc_mint, token::authority = pool)]
    pub pool_usdc: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub reputation_program: Program<'info, Reputation>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct MarkDefault<'info> {
    #[account(mut, seeds = [b"pool"], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"credit", code_hash.as_ref()],
        bump = credit_line.bump,
    )]
    pub credit_line: Account<'info, CreditLine>,

    /// VouchPool to drain. Address validated by Reputation's PDA seeds.
    #[account(mut)]
    pub vouch_pool: Account<'info, VouchPool>,

    #[account(mut)]
    pub rep_stats: Account<'info, RepStats>,

    #[account(mut, token::mint = usdc_mint, token::authority = vouch_pool)]
    pub vouch_pool_usdc: Account<'info, TokenAccount>,

    /// Pool USDC ATA — receives the 50% recovery.
    #[account(mut, token::mint = usdc_mint, token::authority = pool)]
    pub pool_usdc: Account<'info, TokenAccount>,

    /// Burn ATA — receives the 50% destroyed.
    #[account(mut, token::mint = usdc_mint)]
    pub burn_usdc: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub reputation_program: Program<'info, Reputation>,

    /// Caller pays gas. Anyone can mark a default — there's no privilege check.
    #[account(mut)]
    pub caller: Signer<'info>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[account]
pub struct Pool {
    pub admin: Pubkey,
    pub usdc_mint: Pubkey,
    pub usdc_vault: Pubkey,
    pub reputation_program: Pubkey,
    pub total_deposits: u64,
    pub total_shares: u64,
    pub total_drawn: u64,
    pub total_interest_accrued: u64,
    pub total_interest_repaid: u64,
    pub total_principal_repaid: u64,
    pub total_defaulted_principal: u64,
    pub total_lp_haircut: u64,
    pub default_after_seconds: i64,
    pub bump: u8,
}
impl Pool {
    pub const SIZE: usize = 32 * 4 + 8 * 8 + 8 + 1;
}

#[account]
pub struct DepositRec {
    pub lp: Pubkey,
    pub shares: u64,
    pub deposited_at: i64,
    pub bump: u8,
}
impl DepositRec {
    pub const SIZE: usize = 32 + 8 + 8 + 1;
}

#[account]
pub struct CreditLine {
    pub agent_code_hash: [u8; 32],
    pub agent_wallet_pda: Pubkey,
    pub principal: u64,
    pub accrued_interest: u64,
    pub apr_bps: u16,
    pub last_accrual: i64,
    pub last_repay_at: i64,
    pub opened_at: i64,
    pub is_active: bool,
    pub bump: u8,
}
impl CreditLine {
    pub const SIZE: usize = 32 + 32 + 8 + 8 + 2 + 8 + 8 + 8 + 1 + 1;
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[event]
pub struct Deposited {
    pub lp: Pubkey,
    pub amount: u64,
    pub shares_minted: u64,
    pub new_total_deposits: u64,
    pub new_total_shares: u64,
}

#[event]
pub struct Withdrawn {
    pub lp: Pubkey,
    pub shares_burned: u64,
    pub amount_returned: u64,
    pub new_total_deposits: u64,
    pub new_total_shares: u64,
}

#[event]
pub struct Borrowed {
    pub code_hash: [u8; 32],
    pub amount: u64,
    pub new_principal: u64,
    pub apr_bps: u16,
    pub score_bps: u16,
}

#[event]
pub struct Repaid {
    pub code_hash: [u8; 32],
    pub interest_paid: u64,
    pub principal_paid: u64,
    pub voucher_yield: u64,
    pub line_closed: bool,
}

#[event]
pub struct Defaulted {
    pub code_hash: [u8; 32],
    pub principal_lost: u64,
    pub interest_unpaid: u64,
    pub recovered_from_vouchers: u64,
    pub lp_haircut: u64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[error_code]
pub enum CreditError {
    #[msg("Agent reputation is insufficient to borrow.")]
    InsufficientReputation,
    #[msg("Amount exceeds the agent's credit limit.")]
    AmountExceedsLimit,
    #[msg("Pool has no liquidity (or none available beyond drawn).")]
    PoolEmpty,
    #[msg("Pool has insufficient liquidity for this withdrawal/borrow.")]
    InsufficientLiquidity,
    #[msg("Interest accrual overflowed.")]
    AccrualOverflow,
    #[msg("Credit line is not eligible to be marked defaulted.")]
    NotDefaulted,
    #[msg("Caller is not authorised.")]
    Unauthorized,
    #[msg("Reputation account does not match the canonical PDA for this code_hash.")]
    InvalidReputationAccount,
    #[msg("Agent wallet PDA does not match the one recorded on the credit line.")]
    WalletMismatch,
    #[msg("Repayment exceeds the line's outstanding balance.")]
    OverPayment,
    #[msg("Credit line is closed or never opened.")]
    NoActiveLine,
    #[msg("Amount must be greater than zero.")]
    ZeroAmount,
    #[msg("Resulting share quantity is zero (deposit too small relative to pool).")]
    ZeroShares,
    #[msg("LP does not have enough shares.")]
    InsufficientShares,
    #[msg("Arithmetic overflow.")]
    ArithmeticOverflow,
}
