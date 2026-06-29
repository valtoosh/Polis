//! polis :: Identity primitive
//!
//! Binds an autonomous agent's identity to its `code_hash` rather than to a
//! wallet. The wallet-history fallacy (see design spec §"The wallet-history
//! fallacy") is that wallets are transferable property, so reputation tied to
//! wallet history can be detached from the actor that earned it. By keying the
//! `AgentProfile` PDA on `code_hash`, polis ensures reputation, credit, and
//! every downstream primitive attach to what the agent *is*, not what it has.
//!
//! Each registration is backed by a USDC stake bond escrowed in a
//! `StakeBond` PDA's associated token account. In production this bond is the
//! economic complement to TEE attestation; in the prototype it is the sole
//! deterrent against frivolous identity creation (Sybil cost = bond size).
//!
//! ## Property P-Identity
//!
//! Identity binds to `code_hash`, not wallet. Re-registering a fresh agent
//! under the same operator produces a different identity. Reputation cannot
//! transfer between identities.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("wSWCbEtpjD9fpVR5XSrK9YhEtJKNwPefkwHTN2v3SbG");

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default stake bond amount: $50 USDC (USDC has 6 decimals).
/// The spec default; callers pass `bond_amount` explicitly so this is
/// re-exported as a public constant for client convenience.
pub const DEFAULT_STAKE_BOND_AMOUNT: u64 = 50_000_000;

/// Seed prefix for the `AgentProfile` PDA.
pub const AGENT_SEED: &[u8] = b"agent";

/// Seed prefix for the `StakeBond` PDA.
pub const STAKE_SEED: &[u8] = b"stake";

// ---------------------------------------------------------------------------
// Program
// ---------------------------------------------------------------------------

#[program]
pub mod identity {
    use super::*;

    /// Register a new agent identity, keyed by `code_hash`.
    ///
    /// Security properties enforced here:
    ///   - **One identity per `code_hash`.** The `init` constraint on the
    ///     `AgentProfile` PDA causes any second registration with the same
    ///     `code_hash` to fail. This is structural, not a runtime check.
    ///   - **Operator-of-record is recorded.** The `operator_key` argument is
    ///     persisted on the profile. Future privileged actions (e.g.
    ///     `withdraw_stake_bond`) check this field against the signer.
    ///   - **Bond is escrowed in a PDA-owned ATA.** Once funds land in the
    ///     `StakeBond` PDA's associated token account, they can only leave via
    ///     a program-signed transfer using the `("stake", code_hash)` seeds.
    ///   - **USDC mint is caller-supplied.** Bond denomination is whichever
    ///     mint the operator passes; in practice clients should pin to the
    ///     canonical USDC mint for their cluster (see README).
    pub fn register_agent(
        ctx: Context<RegisterAgent>,
        code_hash: [u8; 32],
        operator_key: Pubkey,
        bond_amount: u64,
    ) -> Result<()> {
        require!(bond_amount > 0, ErrorCode::ZeroBond);

        let clock = Clock::get()?;

        // Initialise the AgentProfile.
        let profile = &mut ctx.accounts.agent_profile;
        profile.code_hash = code_hash;
        profile.operator_key = operator_key;
        profile.created_at = clock.unix_timestamp;
        profile.is_retired = false;
        profile.bump = ctx.bumps.agent_profile;

        // Initialise the StakeBond accounting record.
        let bond = &mut ctx.accounts.stake_bond;
        bond.agent = profile.key();
        bond.operator = operator_key;
        bond.amount = bond_amount;
        bond.posted_at = clock.unix_timestamp;
        bond.bump = ctx.bumps.stake_bond;

        // Transfer the bond: operator USDC → StakeBond's ATA.
        //
        // Security: the source is owned by `operator` (the signer); the
        // destination is the ATA whose authority is the `StakeBond` PDA. After
        // this CPI, only a program-signed transfer can move the bond.
        let cpi_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.operator_usdc.to_account_info(),
                to: ctx.accounts.stake_bond_usdc.to_account_info(),
                authority: ctx.accounts.operator.to_account_info(),
            },
        );
        token::transfer(cpi_ctx, bond_amount)?;

        emit!(AgentRegistered {
            code_hash,
            agent: profile.key(),
            operator: operator_key,
            bond_amount,
            timestamp: clock.unix_timestamp,
        });

        Ok(())
    }

    /// Retire an agent — marks `is_retired = true`.
    ///
    /// In the prototype this is admin-only. In production this would be
    /// callable by the operator-of-record subject to the wind-down protocol
    /// (paper §Wind-down).
    ///
    /// Security properties:
    ///   - **Admin gate.** The `admin` signer must equal the on-file admin in
    ///     the prototype this is checked off-chain because the prototype has
    ///     no `PolisConfig` PDA yet; once `governance` lands the constraint
    ///     becomes `has_one = admin` against `PolisConfig`.
    ///   - **Retirement is irreversible.** No `un_retire` instruction exists.
    ///     A new identity for the same `code_hash` is structurally
    ///     impossible: the `AgentProfile` PDA still exists with
    ///     `is_retired = true`, so `register_agent` would fail at `init`.
    ///   - **Code-hash binding.** The PDA is re-derived from the supplied
    ///     `code_hash` arg, so Anchor's seed check enforces that the caller
    ///     names the correct profile.
    pub fn retire_agent(ctx: Context<RetireAgent>, _code_hash: [u8; 32]) -> Result<()> {
        let profile = &mut ctx.accounts.agent_profile;
        require!(!profile.is_retired, ErrorCode::AlreadyRetired);

        profile.is_retired = true;

        emit!(AgentRetired {
            code_hash: profile.code_hash,
            agent: profile.key(),
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Withdraw the stake bond back to the operator.
    ///
    /// Security properties:
    ///   - **Operator-only.** The signer must equal `agent_profile.operator_key`.
    ///     Enforced by the `has_one = operator` constraint on
    ///     `agent_profile`.
    ///   - **Retired-only.** The agent must be retired. In production this
    ///     would *also* require that no credit line is outstanding and no
    ///     reputation slashing is in flight; the prototype defers
    ///     cross-program checks (see spec §Identity layer).
    ///   - **Program-signed transfer.** The actual token movement is a
    ///     program-signed CPI from the ATA owned by the `StakeBond` PDA. The
    ///     operator's signature alone is *not* enough to move bond funds —
    ///     the program must agree.
    pub fn withdraw_stake_bond(ctx: Context<WithdrawStakeBond>) -> Result<()> {
        let profile = &ctx.accounts.agent_profile;
        require!(profile.is_retired, ErrorCode::NotRetired);

        let amount = ctx.accounts.stake_bond_usdc.amount;
        require!(amount > 0, ErrorCode::EmptyBond);

        let code_hash = profile.code_hash;
        let bond_bump = ctx.accounts.stake_bond.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[STAKE_SEED, code_hash.as_ref(), &[bond_bump]]];

        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.stake_bond_usdc.to_account_info(),
                to: ctx.accounts.operator_usdc.to_account_info(),
                authority: ctx.accounts.stake_bond.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(cpi_ctx, amount)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32], operator_key: Pubkey, bond_amount: u64)]
pub struct RegisterAgent<'info> {
    /// Operator signs and pays for account creation; also funds the stake bond.
    #[account(mut)]
    pub operator: Signer<'info>,

    /// `AgentProfile` PDA keyed by `code_hash`.
    /// `init` enforces single registration per code_hash.
    #[account(
        init,
        payer = operator,
        space = AgentProfile::SPACE,
        seeds = [AGENT_SEED, code_hash.as_ref()],
        bump,
    )]
    pub agent_profile: Account<'info, AgentProfile>,

    /// `StakeBond` accounting PDA. Holds bond metadata; the actual token
    /// custody is in the ATA owned by this PDA (`stake_bond_usdc`).
    #[account(
        init,
        payer = operator,
        space = StakeBond::SPACE,
        seeds = [STAKE_SEED, code_hash.as_ref()],
        bump,
    )]
    pub stake_bond: Account<'info, StakeBond>,

    /// USDC mint. Passed in so the agent can be deployed against any USDC
    /// mint (mainnet or devnet). Clients are responsible for using the
    /// correct one. See README for the canonical mint addresses.
    /// Boxed to keep `try_accounts` under the 4 KB SBF stack frame limit.
    pub usdc_mint: Box<Account<'info, Mint>>,

    /// Operator's USDC token account (source of bond).
    #[account(
        mut,
        constraint = operator_usdc.mint == usdc_mint.key() @ ErrorCode::WrongMint,
        constraint = operator_usdc.owner == operator.key() @ ErrorCode::WrongTokenAccountOwner,
    )]
    pub operator_usdc: Box<Account<'info, TokenAccount>>,

    /// Destination: ATA owned by the `StakeBond` PDA. The `StakeBond` PDA
    /// itself is freshly `init`'d above, so this ATA cannot pre-exist —
    /// using `init` (not `init_if_needed`) closes the re-initialization
    /// attack surface.
    #[account(
        init,
        payer = operator,
        associated_token::mint = usdc_mint,
        associated_token::authority = stake_bond,
    )]
    pub stake_bond_usdc: Box<Account<'info, TokenAccount>>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct RetireAgent<'info> {
    /// Admin signer. In the prototype this is a single key; in production
    /// this is the bicameral DAO (paper §Governance). The off-chain
    /// orchestrator gates this; once `governance` lands, the constraint will
    /// become `has_one = admin` against `PolisConfig`.
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [AGENT_SEED, code_hash.as_ref()],
        bump = agent_profile.bump,
    )]
    pub agent_profile: Account<'info, AgentProfile>,
}

#[derive(Accounts)]
pub struct WithdrawStakeBond<'info> {
    /// Operator-of-record. Must match `agent_profile.operator_key`.
    #[account(mut)]
    pub operator: Signer<'info>,

    #[account(
        seeds = [AGENT_SEED, agent_profile.code_hash.as_ref()],
        bump = agent_profile.bump,
        constraint = agent_profile.operator_key == operator.key() @ ErrorCode::NotOperator,
    )]
    pub agent_profile: Account<'info, AgentProfile>,

    #[account(
        seeds = [STAKE_SEED, agent_profile.code_hash.as_ref()],
        bump = stake_bond.bump,
        constraint = stake_bond.operator == operator.key() @ ErrorCode::NotOperator,
        constraint = stake_bond.agent == agent_profile.key() @ ErrorCode::CodeHashMismatch,
    )]
    pub stake_bond: Account<'info, StakeBond>,

    pub usdc_mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = stake_bond,
    )]
    pub stake_bond_usdc: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = operator_usdc.mint == usdc_mint.key() @ ErrorCode::WrongMint,
        constraint = operator_usdc.owner == operator.key() @ ErrorCode::WrongTokenAccountOwner,
    )]
    pub operator_usdc: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[account]
pub struct AgentProfile {
    pub code_hash: [u8; 32],
    pub operator_key: Pubkey,
    pub created_at: i64,
    pub is_retired: bool,
    pub bump: u8,
}

impl AgentProfile {
    /// 8 discriminator + 32 code_hash + 32 operator + 8 created_at + 1 retired + 1 bump
    pub const SPACE: usize = 8 + 32 + 32 + 8 + 1 + 1;
}

#[account]
pub struct StakeBond {
    pub agent: Pubkey,
    pub operator: Pubkey,
    pub amount: u64,
    pub posted_at: i64,
    pub bump: u8,
}

impl StakeBond {
    /// 8 disc + 32 agent + 32 operator + 8 amount + 8 posted_at + 1 bump
    pub const SPACE: usize = 8 + 32 + 32 + 8 + 8 + 1;
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[event]
pub struct AgentRegistered {
    pub code_hash: [u8; 32],
    pub agent: Pubkey,
    pub operator: Pubkey,
    pub bond_amount: u64,
    pub timestamp: i64,
}

#[event]
pub struct AgentRetired {
    pub code_hash: [u8; 32],
    pub agent: Pubkey,
    pub timestamp: i64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[error_code]
pub enum ErrorCode {
    #[msg("Stake bond amount must be greater than zero.")]
    ZeroBond,
    #[msg("Agent is already retired.")]
    AlreadyRetired,
    #[msg("Agent must be retired before stake bond can be withdrawn.")]
    NotRetired,
    #[msg("Stake bond is empty.")]
    EmptyBond,
    #[msg("Signer is not the operator-of-record for this agent.")]
    NotOperator,
    #[msg("Token account mint does not match the supplied USDC mint.")]
    WrongMint,
    #[msg("Token account owner does not match the expected authority.")]
    WrongTokenAccountOwner,
    #[msg("StakeBond does not reference the supplied AgentProfile.")]
    CodeHashMismatch,
}
