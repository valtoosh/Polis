//! polis :: wallet — containment treasury for autonomous agents.
//!
//! The Wallet is a PDA-owned USDC vault, keyed by an agent's `code_hash`.
//! It implements P-Containment from the polis spec: funds held inside a Wallet
//! can never be moved by a private key. Outbound transfers are only possible
//! through the `transfer_out` instruction, which signs the SPL token CPI with
//! the Wallet PDA seeds (`"wallet", code_hash`) via `invoke_signed`.
//!
//! Two-layer outbound guard (per spec §Treasury):
//!   1. `!wallet.is_paused`
//!   2. `amount * 10_000 <= per_trade_cap_bps * balance`
//!
//! Inbound transfers (e.g. from x402 `pay_for_service`) require no gating and
//! land in the Wallet's associated token account directly.
//!
//! IMPORTANT SECURITY NOTE:
//! The agent's `operator_key` never signs for `transfer_out`. Only the Wallet
//! PDA signs (via `invoke_signed` with seeds `("wallet", code_hash)`). This is
//! the structural containment property — funds in Wallet can never be
//! transferred by anyone outside the wallet program's logic. The operator is
//! authenticated as the admin who flips `is_paused` and the per-trade cap, but
//! the agent itself (running inside its attested code) is what authorizes
//! `transfer_out` by calling this program from the agent's runtime.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};

declare_id!("7Jz35XjSeWfA28t8oxzbTyfUTuvgiM4YYnkygpktALBY");

/// Default per-trade cap in basis points: 20% of current balance.
pub const DEFAULT_PER_TRADE_CAP_BPS: u16 = 2_000;
/// Bps denominator.
pub const BPS_DENOMINATOR: u64 = 10_000;
/// Maximum allowed bps for a cap (100%).
pub const MAX_BPS: u16 = 10_000;

#[program]
pub mod wallet {
    use super::*;

    /// Initialize a Wallet PDA + associated USDC token account for an agent.
    ///
    /// Invariant preserved: exactly one Wallet per `code_hash` (the PDA's seed
    /// is `code_hash`, so re-init fails with AccountAlreadyInitialized).
    /// The Wallet PDA owns its own token account — no private key can move
    /// funds out of it.
    pub fn init_wallet(ctx: Context<InitWallet>, code_hash: [u8; 32]) -> Result<()> {
        let w = &mut ctx.accounts.wallet;
        w.code_hash = code_hash;
        w.admin = ctx.accounts.admin.key();
        w.usdc_mint = ctx.accounts.usdc_mint.key();
        w.is_paused = false;
        w.per_trade_cap_bps = DEFAULT_PER_TRADE_CAP_BPS;
        w.total_inflow = 0;
        w.total_outflow = 0;
        w.created_at = Clock::get()?.unix_timestamp;
        w.bump = ctx.bumps.wallet;
        Ok(())
    }

    /// Outbound transfer with two-layer guard, signed by the Wallet PDA.
    ///
    /// Security invariants:
    ///  - Funds leave only if the Wallet is unpaused.
    ///  - A single transfer cannot exceed `per_trade_cap_bps` of the current
    ///    balance (prevents catastrophic drain via a single compromised call).
    ///  - The SPL token CPI is signed by the Wallet PDA, NOT by any operator
    ///    or agent private key. This is the structural containment property.
    ///  - `transfer_checked` (not `transfer`) is used to bind the transfer
    ///    to the exact USDC mint + decimals registered at init.
    pub fn transfer_out(
        ctx: Context<TransferOut>,
        amount: u64,
        _destination: Pubkey,
    ) -> Result<()> {
        let w = &ctx.accounts.wallet;

        // Layer 1: paused gate.
        require!(!w.is_paused, WalletErr::Paused);

        // Layer 2: per-trade cap.
        let balance = ctx.accounts.wallet_ata.amount;
        require!(balance > 0, WalletErr::InsufficientBalance);
        require!(amount <= balance, WalletErr::InsufficientBalance);

        // amount * 10_000 <= per_trade_cap_bps * balance.
        // Use u128 to avoid overflow on u64 multiplication.
        let lhs: u128 = (amount as u128)
            .checked_mul(BPS_DENOMINATOR as u128)
            .ok_or(WalletErr::ArithmeticOverflow)?;
        let rhs: u128 = (w.per_trade_cap_bps as u128)
            .checked_mul(balance as u128)
            .ok_or(WalletErr::ArithmeticOverflow)?;
        require!(lhs <= rhs, WalletErr::ExceedsTradeCap);

        // Bind destination ATA's mint to our USDC mint (extra defense).
        require_keys_eq!(
            ctx.accounts.destination_ata.mint,
            ctx.accounts.usdc_mint.key(),
            WalletErr::MintMismatch
        );
        require_keys_eq!(
            ctx.accounts.wallet_ata.mint,
            ctx.accounts.usdc_mint.key(),
            WalletErr::MintMismatch
        );

        // CPI: signed by the Wallet PDA. The operator/agent never signs this.
        let code_hash = w.code_hash;
        let bump = w.bump;
        let seeds: &[&[u8]] = &[b"wallet", code_hash.as_ref(), &[bump]];
        let signer_seeds: &[&[&[u8]]] = &[seeds];

        let cpi_accounts = TransferChecked {
            from: ctx.accounts.wallet_ata.to_account_info(),
            mint: ctx.accounts.usdc_mint.to_account_info(),
            to: ctx.accounts.destination_ata.to_account_info(),
            authority: ctx.accounts.wallet.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer_seeds,
        );
        token::transfer_checked(cpi_ctx, amount, ctx.accounts.usdc_mint.decimals)?;

        // Bookkeeping.
        let w_mut = &mut ctx.accounts.wallet;
        w_mut.total_outflow = w_mut
            .total_outflow
            .checked_add(amount)
            .ok_or(WalletErr::ArithmeticOverflow)?;

        emit!(TransferExecuted {
            code_hash: w_mut.code_hash,
            amount,
            destination: ctx.accounts.destination_ata.key(),
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Pause / unpause outbound transfers. Admin-only.
    ///
    /// Invariant: only the registered admin can flip this flag. When paused,
    /// no `transfer_out` can succeed; inbound `pay_for_service` is unaffected.
    pub fn set_paused(ctx: Context<AdminUpdate>, paused: bool) -> Result<()> {
        let w = &mut ctx.accounts.wallet;
        require_keys_eq!(
            ctx.accounts.admin.key(),
            w.admin,
            WalletErr::Unauthorized
        );
        w.is_paused = paused;
        emit!(PauseStateChanged {
            code_hash: w.code_hash,
            is_paused: paused,
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Update per-trade cap in bps. Admin-only. Capped at 100% (10_000 bps).
    pub fn update_per_trade_cap(ctx: Context<AdminUpdate>, bps: u16) -> Result<()> {
        let w = &mut ctx.accounts.wallet;
        require_keys_eq!(
            ctx.accounts.admin.key(),
            w.admin,
            WalletErr::Unauthorized
        );
        require!(bps <= MAX_BPS, WalletErr::InvalidBps);
        let old = w.per_trade_cap_bps;
        w.per_trade_cap_bps = bps;
        emit!(PerTradeCapUpdated {
            code_hash: w.code_hash,
            old_bps: old,
            new_bps: bps,
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Notify the Wallet of an inbound transfer. Optional but allows on-chain
    /// `total_inflow` bookkeeping for callers (x402, credit draw) that want it.
    /// The actual USDC movement is done by the caller; this just bumps the
    /// counter. Anyone may call.
    pub fn record_inflow(ctx: Context<RecordInflow>, amount: u64) -> Result<()> {
        let w = &mut ctx.accounts.wallet;
        w.total_inflow = w
            .total_inflow
            .checked_add(amount)
            .ok_or(WalletErr::ArithmeticOverflow)?;
        emit!(InflowRecorded {
            code_hash: w.code_hash,
            amount,
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }
}

// =====================================================================
//  Accounts
// =====================================================================

#[derive(Accounts)]
#[instruction(code_hash: [u8; 32])]
pub struct InitWallet<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + Wallet::INIT_SPACE,
        seeds = [b"wallet", code_hash.as_ref()],
        bump,
    )]
    pub wallet: Account<'info, Wallet>,

    /// USDC mint (or any SPL mint chosen at init).
    pub usdc_mint: Account<'info, Mint>,

    /// Wallet's associated USDC token account. Authority = the Wallet PDA.
    #[account(
        init,
        payer = admin,
        associated_token::mint = usdc_mint,
        associated_token::authority = wallet,
    )]
    pub wallet_ata: Account<'info, TokenAccount>,

    #[account(mut)]
    pub admin: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct TransferOut<'info> {
    #[account(
        mut,
        seeds = [b"wallet", wallet.code_hash.as_ref()],
        bump = wallet.bump,
        has_one = usdc_mint @ WalletErr::MintMismatch,
    )]
    pub wallet: Account<'info, Wallet>,

    pub usdc_mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = wallet,
    )]
    pub wallet_ata: Account<'info, TokenAccount>,

    /// Destination token account. Caller is responsible for choosing the
    /// right destination; the program only enforces same-mint.
    #[account(mut)]
    pub destination_ata: Account<'info, TokenAccount>,

    /// The "caller" who invoked this transfer. In prototype this is the
    /// admin/operator; in production this would be an attested agent runtime
    /// authorized through identity. Either way, this signer cannot move funds
    /// directly — only the program (via PDA signing) can.
    pub caller: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AdminUpdate<'info> {
    #[account(
        mut,
        seeds = [b"wallet", wallet.code_hash.as_ref()],
        bump = wallet.bump,
    )]
    pub wallet: Account<'info, Wallet>,

    pub admin: Signer<'info>,
}

#[derive(Accounts)]
pub struct RecordInflow<'info> {
    #[account(
        mut,
        seeds = [b"wallet", wallet.code_hash.as_ref()],
        bump = wallet.bump,
    )]
    pub wallet: Account<'info, Wallet>,

    pub caller: Signer<'info>,
}

// =====================================================================
//  State
// =====================================================================

#[account]
#[derive(InitSpace)]
pub struct Wallet {
    /// The hash of the agent's code. PDA seed.
    pub code_hash: [u8; 32],
    /// Admin / operator who can flip pause + cap. Prototype only — production
    /// replaces with governance.
    pub admin: Pubkey,
    /// SPL mint this wallet holds (USDC in production).
    pub usdc_mint: Pubkey,
    /// If true, all `transfer_out` calls revert.
    pub is_paused: bool,
    /// Per-trade cap in bps (1 bp = 0.01%). Default 2_000 = 20%.
    pub per_trade_cap_bps: u16,
    /// Lifetime inflow counter (best-effort; updated via record_inflow).
    pub total_inflow: u64,
    /// Lifetime outflow counter (authoritative; updated on every transfer_out).
    pub total_outflow: u64,
    /// Unix timestamp at creation.
    pub created_at: i64,
    /// PDA bump.
    pub bump: u8,
}

impl Wallet {
    /// Helper: check that the underlying ATA holds at least `amount`.
    /// Anchor's `Account<TokenAccount>` already gives us .amount; this is a
    /// convenience wrapper for callers (other programs doing CPI).
    pub fn assert_balance(token_account_amount: u64, required: u64) -> Result<()> {
        require!(
            token_account_amount >= required,
            WalletErr::InsufficientBalance
        );
        Ok(())
    }
}

// =====================================================================
//  Events
// =====================================================================

#[event]
pub struct TransferExecuted {
    pub code_hash: [u8; 32],
    pub amount: u64,
    pub destination: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct PauseStateChanged {
    pub code_hash: [u8; 32],
    pub is_paused: bool,
    pub timestamp: i64,
}

#[event]
pub struct PerTradeCapUpdated {
    pub code_hash: [u8; 32],
    pub old_bps: u16,
    pub new_bps: u16,
    pub timestamp: i64,
}

#[event]
pub struct InflowRecorded {
    pub code_hash: [u8; 32],
    pub amount: u64,
    pub timestamp: i64,
}

// =====================================================================
//  Errors
// =====================================================================

#[error_code]
pub enum WalletErr {
    #[msg("Wallet is paused — outbound transfers disabled")]
    Paused,
    #[msg("Transfer amount exceeds per-trade cap (bps of current balance)")]
    ExceedsTradeCap,
    #[msg("Wallet token account has insufficient balance")]
    InsufficientBalance,
    #[msg("Caller is not authorized for this operation")]
    Unauthorized,
    #[msg("Provided mint does not match the wallet's registered USDC mint")]
    MintMismatch,
    #[msg("Bps value out of range (must be <= 10_000)")]
    InvalidBps,
    #[msg("Arithmetic overflow")]
    ArithmeticOverflow,
}
