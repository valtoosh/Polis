# polis — Phase 1 Integration Status

**Date:** 2026-06-04
**Phase:** Initial parallel scaffolding (5 sub-agents)
**Status:** ✅ All programs compile, scripts typecheck clean, agent AST-parses clean

---

## What's in the tree

```
~/polis/
├── design/2026-06-04-polis-spec.md       <- 700-line approved spec
├── programs/                              <- 5 Anchor crates, 2,920 LOC of Rust
│   ├── identity/        385 LOC          <- ★ full impl, compiles
│   ├── wallet/          393 LOC          <- ★ full impl, compiles
│   ├── x402/            225 LOC          <- ★ full impl, compiles
│   ├── credit/        1,071 LOC          <- ★ full impl, compiles
│   └── reputation/      846 LOC          <- ★ full impl, compiles
├── agent/                                 <- Python reference agent, 10 files
│   ├── server.py                          <-   FastAPI /summarize endpoint
│   ├── main.py                            <-   orchestrator + lifecycle
│   ├── solana_client.py                   <-   hand-rolled Anchor wire format
│   ├── treasury_manager.py
│   ├── credit_manager.py
│   ├── llm_client.py                      <-   Claude Haiku 4.5 wrapper
│   ├── code_hash.py                       <-   identity derivation
│   ├── requirements.txt
│   ├── .env.example
│   └── README.md
├── scripts/                               <- 5 TS scripts, tsc-clean
│   ├── _common.ts                         <-   PDA derivation, config IO
│   ├── bootstrap.ts                       <-   devnet one-shot setup
│   ├── simulate-customers.ts              <-   Poisson traffic generator
│   ├── demo-day.ts                        <-   daily snapshotter
│   ├── audit.ts                           <-   14-day pass-criteria verifier
│   └── package.json / tsconfig.json
├── paper/
│   ├── polis.tex                          <- 53 KB compilable LaTeX (~30 pages)
│   ├── polis.md                           <- 26 KB Markdown companion
│   └── README.md
├── Cargo.toml                             <- workspace manifest
├── Anchor.toml                            <- 5 program IDs aligned w/ declare_id!
└── .gitignore
```

---

## Verification

| Check | Result |
|---|---|
| `cargo check --workspace` | ✅ Pass (all 5 programs) |
| `tsc --noEmit` in scripts/ | ✅ Pass |
| Python AST parse, all agent .py files | ✅ Pass |
| `declare_id!` matches Anchor.toml | ✅ Aligned (sha256-derived placeholders) |
| `pdflatex polis.tex` | ⚠️ Not tested locally (no pdflatex installed); structure verified manually |

---

## Placeholder program IDs (sha256-derived; replace after `anchor keys sync`)

| Program | Placeholder ID |
|---|---|
| identity | `83QPXvAQiHEzEoJPXjJikLE6zNYskTJKJ28dBHT88ww9` |
| wallet | `GfsJWjmGXMfct8JMR9Lm9ySUnniZbnGUTQDbT8ipWf9U` |
| x402 | `GF1kQpGNVLNjRp6meTKHGiBSjhQo6hpfCBDAAc9sUt1Q` |
| credit | `GwF2acoetwRJJhC3ryiy8sGG28EdsCWTUq51pTzLm4Y7` |
| reputation | `6kVdPLjzT6URT2sQdHmTwRGSKx6jYDjwMKKVsWxU18Xw` |

---

## Notable design decisions made by sub-agents (beyond the spec)

1. **Wallet stores `admin` + `usdc_mint`** as account state, enabling `has_one`-style constraint enforcement.
2. **x402 verifies the destination Wallet PDA by re-derivation** (rather than CPI'ing into the wallet program). Saves CU; safety equivalent.
3. **Credit↔Reputation coupling** is via Cargo path-dep (no shared `polis-core` crate). Anchor 0.30 CPI idiom.
4. **Reputation uses lazy slashing via generation counter** — slash is O(1), affected vouchers wipe their claim on next touch. Avoids the O(n) write storm.
5. **Reputation yield is Compound-style cumulative index** (u128 fixed-point, 1e12 scale). Joiners post-yield don't earn on pre-existing yield.
6. **Credit's `last_repay_at` is separate from `last_accrual`** — avoids the bug where calling `accrue` would reset the default clock.
7. **Burn is transfer-to-zero-address** (matches spec). Real token burn would require USDC mint authority.
8. **Python agent hand-rolls Anchor wire format** — IDLs don't exist yet, so encoding is `sha256("global:<ix>")[:8]` + borsh, with TODO_IDL tags everywhere.
9. **Customer payment binding via memo field** — `payment_id` is a 32-byte hex passed in the on-chain memo, looked up off-chain via WebSocket event subscription.

---

## Known gaps before demo can run

### Critical (blocks demo)

- [x] **Wallet PDA re-derivation check** in both `x402` and `credit` — DONE. Added `pub const WALLET_PROGRAM_ID` to both programs via `anchor_lang::pubkey!`; `require_keys_eq!` on the supplied wallet_program key + canonical PDA derivation. Workspace still compiles.
- [x] **`anchor build` + `anchor keys sync` + IDL generation** — DONE. All 5 `.so` files in `target/deploy/`; all 5 IDLs in `target/idl/`; `declare_id!` synced to keypair-derived IDs:
  - identity: `BgSaU26SU5RdAxzTNoMFyXoWqxRqyhEcz8zoVzeJVtPJ`
  - wallet: `7vhtmDYP3S96LyV2hbhF4dreeKUBTPseGEAvM7f7oZZg`
  - x402: `AfYrQJzsmKh6hEHiKZKmA2Y3Mdi4P8xMbKTZFNMTh3r8`
  - credit: `BXyFp9aSMLPQJs2iL9zpCwdeccReCJ2eQ99P4CBEUbQr`
  - reputation: `FViSRPkKvkQameAucJtfpV4YJcbG34Yr9urPFSwxrjKH`
  - `WALLET_PROGRAM_ID` constants in x402 + credit updated to new wallet ID.
  - `rust-toolchain.toml` pinned to 1.85 (needed for transitive `wit-bindgen` edition2024 requirement).
- [x] **Devnet USDC faucet helper** — DONE. `scripts/fund-devnet.ts` drips SOL + USDC into operator + LP + voucher + customer keypairs (drains from a "dispenser" keypair the operator pre-funds via https://faucet.circle.com).
- [x] **Credit's wallet-out-flow path** — DONE. Decision documented in `agent/REPAY_FLOW.md`: prototype uses Option A (2-instruction tx: `wallet::transfer_out` + `credit::repay` in the same atomic tx). Production migrates to Option B (`transfer_out_authorized` CPI on wallet).
- [x] **Agent's TODO_IDL → IDL-based calls** — DONE. `agent/idls.py` (new) loads each IDL with a 0.30→0.28 schema converter (anchorpy 0.20.1 limitation). `agent/solana_client.py` drives every chain interaction through `anchorpy.Program`. Scripts (`bootstrap.ts`, `simulate-customers.ts`, `demo-day.ts`, `audit.ts`) use `program.methods.X(...).accounts({...}).rpc()` instead of hand-rolled discriminators. Verification: `python -c "from solana_client import SolanaClient"` clean; `npx tsc --noEmit` clean. Notable drift caught: legacy `transfer_out` accounts list was wrong (corrected to match deployed program); `PolisConfig` PDA dropped (not in deployed identity); reputation max-stake now read from `RepStats.max_total_staked_seen` (O(1) vs O(N) sweep).
- [x] **First devnet deploy** — DONE. All 5 programs deployed on Solana devnet:
  - identity:   `BgSaU26SU5RdAxzTNoMFyXoWqxRqyhEcz8zoVzeJVtPJ`
  - wallet:     `7vhtmDYP3S96LyV2hbhF4dreeKUBTPseGEAvM7f7oZZg`
  - x402:       `AfYrQJzsmKh6hEHiKZKmA2Y3Mdi4P8xMbKTZFNMTh3r8`
  - credit:     `BXyFp9aSMLPQJs2iL9zpCwdeccReCJ2eQ99P4CBEUbQr`
  - reputation: `FViSRPkKvkQameAucJtfpV4YJcbG34Yr9urPFSwxrjKH`
- [x] **Paper sections expanded** — DONE. All `\todoMark{Expanded section.}` placeholders removed (§6 Architecture, §7 Identity, §10 Credit, §11 Reputation, §12 Governance, §15 Demo). Code listings added to Appendix B (eight-layer guard, interest accrual, reputation score).
- [x] **Paper converted to double-column IEEEtran format** — DONE. `polis.tex` rewritten as a proper conference-style manuscript: 2,165 lines, `\documentclass[10pt,conference,letterpaper]{IEEEtran}`. 5 TikZ figures (architecture, HF state machine, x402 sequence, repayment waterfall, bad-debt absorption), 7 booktabs tables, 9 algorithm boxes, 5 theorems with proof sketches, 5 numbered equations, 26 citations. Braces / environments balanced.
- [x] **Cargo warnings fixed** — DONE. `[workspace.lints.rust]` declares `check-cfg` for `target_os=solana` and Anchor's internal feature flags; each program has `[lints] workspace = true`. Zero warnings on full rebuild.
- [ ] **Bootstrap script run** — `scripts/bootstrap.ts` to init Pool, register the reference agent, seed LPs, post initial vouch. Pending IDL agent + dispenser USDC topup.

### Non-blocking (paper-noted, post-demo)

- [ ] Governance — admin-controlled in prototype; bicameral DAO is paper-only.
- [ ] Dispute resolution — assumed happy path; spec'd but not implemented.
- [ ] Wind-down — admin flag in prototype; full settlement is paper-only.
- [ ] TEE attestation for identity — code_hash + stake bond is the prototype primitive.
- [ ] `total_interest_accrued` Pool stat is initialized but never updated (cosmetic).

---

## What to do next

1. **Manual eyeball pass** on each program's lib.rs to confirm spec invariants. Particular focus:
   - Identity: `register_agent` cannot be called twice with the same `code_hash`
   - Wallet: outbound transfer's signed-by-PDA invariant is intact
   - x402: payment events emit the right discriminator (the Python agent depends on this)
   - Credit: interest accrual formula matches Eq. in spec
   - Reputation: slash CPI authority check is sound (Pool PDA signer)

2. **Address the "Critical" list** above. Estimated effort:
   - Wallet PDA re-derivation check: 30 min total across x402 + credit
   - `anchor build` + IDL gen: 1 hour (mostly waiting for build)
   - Agent's `TODO_IDL` → IDL-based: 2 hours
   - Devnet faucet funding script: 1 hour
   - Credit's repay path (option a): 2 hours
   - Total: ~7 hours of focused work

3. **Run smoke locally:**
   - `anchor build` from `~/polis/`
   - `solana-test-validator` running locally
   - `cd scripts && npm install` (already done by sub-agent E)
   - `cd scripts && npm run bootstrap`
   - `cd agent && pip install -r requirements.txt && python main.py`
   - `cd scripts && npm run simulate-customers -- --speedup=100x`

4. **Then 14-day devnet run** for the existence proof.

---

## Total deliverable size

- **Code:** ~2,920 LOC Rust + ~6,500 LOC Python/TS = ~9,400 LOC
- **Paper:** ~30 pages typeset (53 KB LaTeX + 26 KB Markdown)
- **Spec:** ~700 lines Markdown
- **Sub-agent runtime:** 5 agents in parallel, ~6-15 min each (longest: scripts+paper at 16 min)
- **Net wall-clock:** ~16 minutes of parallel compute for Phase 1 scaffolding

---

*End of Phase 1 status report.*
