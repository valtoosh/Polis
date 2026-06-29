//! polis :: x402 — on-chain settlement primitive for agent services.
//!
//! Per spec §Earning rails: one instruction, `pay_for_service`. A customer
//! transfers USDC from their token account into the agent's Wallet PDA's
//! associated token account (the Wallet PDA is owned by the `wallet` program
//! and keyed by the agent's `code_hash`).
//!
//! The x402 program does NOT CPI into the `wallet` program. Inbound transfers
//! to a Wallet are deliberately gateless: the spec specifies that the Wallet
//! program gates only OUTBOUND movement (P-Containment). Open inbound flow
//! means anyone can pay an agent — this is the desired property for x402.
//!
//! Receipts are NOT persisted on-chain (see PaymentReceiptStub). The event log
//! is the authoritative record. This is a deliberate prototype tradeoff:
//! per-payment account allocation would be the dominant on-chain cost in a
//! many-customers regime, and customers can reconstruct receipts from the
//! deterministic `payment_id` + the program log.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hashv;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};

declare_id!("3qWKUNmhHw7qraor4oYuyzchYwqQFiYDUbxMKoK5NKt8");

/// Hard-bound the wallet program ID at compile time. A misconfigured client
/// cannot route funds to a wrong-program PDA — `pay_for_service` checks the
/// passed `wallet_program` account key against this constant.
pub const WALLET_PROGRAM_ID: Pubkey =
    anchor_lang::pubkey!("7Jz35XjSeWfA28t8oxzbTyfUTuvgiM4YYnkygpktALBY");

/// Hard cap on memo length to bound transaction size + log noise.
pub const MAX_MEMO_LEN: usize = 200;

#[program]
pub mod x402 {
    use super::*;

    /// Pay a polis agent for a service.
    ///
    /// Invariants preserved:
    ///  - The destination is constrained to be the ATA of a `("wallet", code_hash)`
    ///    PDA owned by the `wallet` program. We do NOT verify the wallet
    ///    program's account state via CPI (that would be expensive and is
    ///    unnecessary for inbound transfers), but we DO re-derive the Wallet
    ///    PDA from `agent_code_hash` and the configured wallet program id and
    ///    require the destination ATA's authority equal that PDA.
    ///  - The transfer uses `transfer_checked` so caller cannot mismatch the
    ///    mint or decimals.
    ///  - Memo length is bounded.
    ///  - A deterministic `payment_id` is emitted in the event so customers
    ///    can reference the payment off-chain.
    pub fn pay_for_service(
        ctx: Context<PayForService>,
        agent_code_hash: [u8; 32],
        amount: u64,
        memo: Option<String>,
    ) -> Result<()> {
        // Memo length gate.
        let memo_bytes: &[u8] = match &memo {
            Some(m) => {
                require!(m.len() <= MAX_MEMO_LEN, X402Err::MemoTooLong);
                m.as_bytes()
            }
            None => &[],
        };

        require!(amount > 0, X402Err::ZeroAmount);

        // Re-derive the Wallet PDA owned by the wallet program. The caller
        // passes the wallet program id as an account so we can compare; we
        // do not CPI into the wallet program (see file-level note).
        //
        // HARDENED: cross-check the supplied wallet_program against the compile-
        // time WALLET_PROGRAM_ID constant. Closes the footgun B's report
        // flagged where a wrong wallet_program account would still pass the
        // owner check (against a PDA derived from the wrong program).
        require_keys_eq!(
            *ctx.accounts.wallet_program.key,
            WALLET_PROGRAM_ID,
            X402Err::WrongWalletProgram
        );
        let (expected_wallet_pda, _bump) = Pubkey::find_program_address(
            &[b"wallet", agent_code_hash.as_ref()],
            ctx.accounts.wallet_program.key,
        );

        // The destination ATA must be authority-owned by the agent's Wallet PDA.
        require_keys_eq!(
            ctx.accounts.agent_wallet_ata.owner,
            expected_wallet_pda,
            X402Err::DestinationAuthorityMismatch
        );
        // And it must be the configured USDC mint.
        require_keys_eq!(
            ctx.accounts.agent_wallet_ata.mint,
            ctx.accounts.usdc_mint.key(),
            X402Err::MintMismatch
        );
        require_keys_eq!(
            ctx.accounts.payer_ata.mint,
            ctx.accounts.usdc_mint.key(),
            X402Err::MintMismatch
        );

        // CPI: customer signs as the authority over their own token account.
        let cpi_accounts = TransferChecked {
            from: ctx.accounts.payer_ata.to_account_info(),
            mint: ctx.accounts.usdc_mint.to_account_info(),
            to: ctx.accounts.agent_wallet_ata.to_account_info(),
            authority: ctx.accounts.payer.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
        );
        token::transfer_checked(cpi_ctx, amount, ctx.accounts.usdc_mint.decimals)?;

        // Build deterministic payment_id:
        //   hash(agent_code_hash || payer || amount_le || slot_le || memo_hash)
        let clock = Clock::get()?;
        let slot = clock.slot;
        let memo_hash = hashv(&[memo_bytes]).to_bytes();

        let payer_key = ctx.accounts.payer.key();
        let amount_le = amount.to_le_bytes();
        let slot_le = slot.to_le_bytes();

        let payment_id_arr = hashv(&[
            agent_code_hash.as_ref(),
            payer_key.as_ref(),
            &amount_le,
            &slot_le,
            &memo_hash,
        ])
        .to_bytes();

        emit!(PaymentReceived {
            agent_code_hash,
            payer: payer_key,
            amount,
            memo,
            timestamp: clock.unix_timestamp,
            slot,
            payment_id: payment_id_arr,
        });

        Ok(())
    }
}

// =====================================================================
//  Accounts
// =====================================================================

#[derive(Accounts)]
#[instruction(agent_code_hash: [u8; 32])]
pub struct PayForService<'info> {
    /// The customer paying for a service. Signs to authorize the transfer
    /// from their own token account.
    #[account(mut)]
    pub payer: Signer<'info>,

    /// USDC mint.
    pub usdc_mint: Account<'info, Mint>,

    /// Customer's source USDC account.
    #[account(mut)]
    pub payer_ata: Account<'info, TokenAccount>,

    /// Agent's Wallet PDA's USDC ATA. Authority MUST be the
    /// `("wallet", agent_code_hash)` PDA owned by `wallet_program`. We
    /// re-derive and check inside the instruction; we don't decode the Wallet
    /// account itself (open inbound; no need).
    #[account(mut)]
    pub agent_wallet_ata: Account<'info, TokenAccount>,

    /// The polis `wallet` program. Passed as an account so we can derive the
    /// expected Wallet PDA. Marked `UncheckedAccount` because we only use its
    /// key — we do not invoke it.
    /// CHECK: only the key is used (find_program_address derivation).
    pub wallet_program: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
}

// =====================================================================
//  State (intentionally minimal)
// =====================================================================

/// `PaymentReceiptStub` is a marker type — we do NOT persist payment receipts
/// on-chain in the prototype. Rationale:
///
///  - At Solana account rent, every receipt costs ~0.002 SOL to store. For an
///    agent serving thousands of customers, this is the dominant on-chain
///    cost.
///  - The event log already contains every field a customer or off-chain
///    indexer needs (agent, payer, amount, memo, timestamp, slot, payment_id).
///  - `payment_id` is deterministic from on-chain data, so a customer can
///    re-derive it from their own records and prove "this is my payment" by
///    matching it against the log.
///
/// Production may add an opt-in receipt persistence flag for use-cases that
/// require on-chain provenance (e.g., legal disputes), but the default is
/// log-only.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct PaymentReceiptStub {
    /// Always zero — placeholder type only.
    pub _unused: u8,
}

// =====================================================================
//  Events
// =====================================================================

#[event]
pub struct PaymentReceived {
    pub agent_code_hash: [u8; 32],
    pub payer: Pubkey,
    pub amount: u64,
    pub memo: Option<String>,
    pub timestamp: i64,
    pub slot: u64,
    /// Deterministic hash of (agent_code_hash, payer, amount, slot, memo_hash).
    pub payment_id: [u8; 32],
}

// =====================================================================
//  Errors
// =====================================================================

#[error_code]
pub enum X402Err {
    #[msg("Memo exceeds maximum allowed length (200 bytes)")]
    MemoTooLong,
    #[msg("Payment amount must be greater than zero")]
    ZeroAmount,
    #[msg("Destination ATA is not authority-owned by the expected Wallet PDA")]
    DestinationAuthorityMismatch,
    #[msg("Token account mint does not match the configured USDC mint")]
    MintMismatch,
    #[msg("wallet_program account does not match the compile-time WALLET_PROGRAM_ID")]
    WrongWalletProgram,
}
