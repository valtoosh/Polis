# Credit Repayment Flow (Option A — 2-instruction transaction)

## Why this is non-trivial

The wallet program (`programs/wallet/`) gates **outbound** USDC transfers
behind its 8-layer (prototype: 2-layer) guard. The only authority that can
sign for the Wallet PDA's USDC ATA is the wallet program itself, via
`invoke_signed` against seeds `("wallet", code_hash)`.

The credit program (`programs/credit/`) needs to receive a repayment
*from* the Wallet PDA. There is no path where credit's `repay` instruction
can directly pull USDC out of the Wallet — the Wallet PDA cannot sign for
credit, and credit cannot sign for the Wallet.

Two architectural options:

| Option | Mechanic | Pro | Con |
|---|---|---|---|
| **A** — 2-instruction tx | Agent assembles a single transaction containing **(1)** `wallet::transfer_out` (sends USDC to the credit Pool's ATA) followed by **(2)** `credit::repay` (records the payment, verifies the Pool USDC ATA delta) | No new wallet-program code; clean separation of concerns; transaction atomicity guarantees both happen or neither | `repay` must verify the delta out-of-band; uses ~2× the CU vs. a direct CPI |
| **B** — `transfer_out_authorized` | Add a new instruction on the wallet program callable only by the credit program (via `Instructions` sysvar or a dedicated authority field). Credit then CPIs into it during `repay` | Single CPI; tighter coupling; cleaner from credit's side | Requires new wallet-program code, audit of the authority check, redeploy. Cross-program coupling tighter (harder to swap one out). |

**Decision for the prototype: Option A.** The thesis is about the *protocol* not its
optimisation; Option A demonstrates the property without introducing new
attack surface in the wallet program.

## Transaction shape

```
[instruction 0] wallet::transfer_out
    accounts:
      caller            = operator key (signer; pays tx fee)
      wallet            = ("wallet", code_hash) PDA
      wallet_usdc       = Wallet's USDC ATA
      destination_usdc  = Pool's USDC ATA (in credit program)
      usdc_mint         = USDC mint
      token_program     = SPL Token program
    data:
      amount            = principal_repay + interest_repay
      destination       = Pool USDC ATA pubkey

[instruction 1] credit::repay
    accounts:
      pool              = ("pool",) PDA
      pool_usdc         = Pool's USDC ATA   <-- same as instruction 0's destination
      credit_line       = ("credit", code_hash) PDA
      vouch_pool        = ("vouch", code_hash) PDA   (for yield distribution CPI)
      vouch_pool_usdc   = VouchPool's USDC ATA
      rep_stats         = ("rep_stats",) PDA
      reputation_program= reputation program ID
      caller            = operator key (signer)
    data:
      code_hash         = the agent's code_hash
      amount            = same value as instruction 0's amount
```

## How `credit::repay` verifies the delta

Per agent C's design, `repay` reads the Pool's USDC ATA `amount` field
before applying state changes; the SPL transfer in instruction 0 has
already increased the balance by exactly `amount`. `repay` then:

1. Splits the incoming `amount` into interest + principal (interest paid
   first per the spec).
2. CPIs into `reputation::distribute_voucher_yield` for 30% of the interest.
3. Updates the credit line and pool totals.
4. Emits `RepaymentReceived(code_hash, amount, ...)`.

Because both instructions are in the same transaction, Solana's atomicity
guarantees that if `credit::repay` fails (e.g., bad data, frozen pool, etc.)
the wallet's transfer is also rolled back. No partial-state risk.

## Python agent implementation outline

The reference agent (`agent/credit_manager.py`) assembles both
instructions in one call. Pseudocode:

```python
async def repay(self, amount_base_units: int) -> str:
    tx = Transaction()

    # ix 0: wallet::transfer_out — send USDC out of Wallet PDA to Pool ATA
    tx.add(self._build_wallet_transfer_out_ix(
        amount=amount_base_units,
        destination=self.pool_usdc_ata,
    ))

    # ix 1: credit::repay — record the payment, distribute voucher yield
    tx.add(self._build_credit_repay_ix(
        code_hash=self.code_hash,
        amount=amount_base_units,
    ))

    sig = await self.solana_client.send_and_confirm(tx, signers=[self.operator_key])
    return sig
```

The operator key signs both — it's the tx fee payer; it is NOT the authority
for the Wallet PDA's USDC ATA. The wallet program's `invoke_signed` (using
the Wallet PDA seeds) is what actually authorises the transfer. The wallet
program's 2-layer guard runs in `instruction 0` before the SPL transfer.

## Why this satisfies P-Containment

The containment property says "no execution path moves funds out of `W_a`
except through the 8-layer (prototype: 2-layer) guard". Option A satisfies
this directly: the only path through which USDC leaves Wallet during
repay is `wallet::transfer_out` itself, which still runs the full guard.
The credit program never bypasses the guard — it just reads the post-state.

## Why this satisfies G3 (loss ordering)

If the agent fails to repay (whether due to insufficient balance, network
issues, or malice), `credit::mark_default` is the only path that touches
LP capital. Loss ordering is enforced inside `mark_default`'s single CPI
sequence (Insurance → Junior → Mezzanine → Senior, abstracted in our
single-tranche prototype to Insurance → LP haircut). Repayment path
never touches the loss-ordering code.

## Failure modes and recovery

| Failure | Effect | Recovery |
|---|---|---|
| ix 0 fails (e.g., per-trade cap exceeded) | Whole tx reverts; no state change | Agent reduces repayment amount, retries |
| ix 1 fails (e.g., pool state mismatch) | Whole tx reverts; transfer rolled back | Operator inspects logs; usually a stale `credit_line` PDA cache |
| Tx times out / dropped | No state change; balance unchanged | Agent retries with fresh blockhash |
| Insufficient USDC in Wallet | ix 0 fails at SPL `transfer_checked` | Agent waits for more inflow before retrying |

## Notes for downstream implementers

- Option B (`transfer_out_authorized` on wallet) is the right production
  choice once the credit-program identity is stable enough to hard-code.
  Migration path: deploy a new wallet program version with the extra
  instruction; the credit program adds CPI in its repay path; the 2-ix
  pattern remains as a fallback for backwards compatibility.
- The pool USDC ATA address is deterministic — it's the ATA of the Pool
  PDA over the USDC mint. Anchor's `init_associated_token_account` in
  `init_pool` set it up at deploy time.
- For high-frequency repayments (sub-minute), the 2-ix pattern's extra
  CU cost can become noticeable. Option B's single-CPI path saves
  approximately 30k CU per repayment.
