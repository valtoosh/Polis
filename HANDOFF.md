# polis — Handoff

Read this first if you are picking up this project without recent context.

## The thesis (verbatim)

> AI agents will become self-sustaining earners without a human in the loop,
> with the help of DeFi.

This is a research project, not a startup. The goal is a defensible
academic claim, backed by a working reference implementation that other
builders can extend.

## The keystone insight

**Wallet-history-based reputation is fundamentally broken for autonomous
agents.** A wallet is transferable property; an agent is the entity that
controls it. An adversary can assign a fresh agent a wallet with a year of
clean on-chain history, and any wallet-history-based score returns
"trustworthy."

Therefore, identity (and everything downstream — reputation, credit,
governance) must bind to **what the agent IS** (its `code_hash`), not to
**what it HAS** (a wallet). This is the load-bearing claim of the paper
and the design choice from which everything else falls out.

## Project state

- 5 Anchor programs, all compile clean (zero warnings), all deployed on
  Solana devnet.
- Reference Python agent + TS scripts driven through IDL-based calls.
- Paper: 2,165-line IEEEtran double-column manuscript with 5 TikZ figures,
  7 booktabs tables, 9 algorithm boxes, 5 theorems with proof sketches,
  26 citations.
- Design spec: 700-line Markdown contract between paper and prototype.
- Status tracker: `STATUS.md` in this directory.

## Live program IDs (Solana devnet)

```
identity:   BgSaU26SU5RdAxzTNoMFyXoWqxRqyhEcz8zoVzeJVtPJ
wallet:     7vhtmDYP3S96LyV2hbhF4dreeKUBTPseGEAvM7f7oZZg
x402:       AfYrQJzsmKh6hEHiKZKmA2Y3Mdi4P8xMbKTZFNMTh3r8
credit:     BXyFp9aSMLPQJs2iL9zpCwdeccReCJ2eQ99P4CBEUbQr
reputation: FViSRPkKvkQameAucJtfpV4YJcbG34Yr9urPFSwxrjKH
```

## Folder layout

```
~/polis/
├── HANDOFF.md           <- this file
├── README.md            <- short overview
├── STATUS.md            <- detailed progress + remaining work
├── design/
│   └── 2026-06-04-polis-spec.md
├── programs/
│   ├── identity/        ★ implemented
│   ├── wallet/          ★ implemented
│   ├── x402/            ★ implemented
│   ├── credit/          ★ implemented
│   └── reputation/      ★ implemented
├── agent/               Python reference agent (IDL-based)
├── scripts/             TS scripts (bootstrap, simulate, demo-day, audit, fund-devnet)
├── paper/
│   ├── polis.tex        2,165-line double-column IEEEtran manuscript
│   ├── polis.md         Markdown companion (single-column)
│   └── README.md
├── target/
│   ├── deploy/          5 .so + 5 keypair JSON
│   └── idl/             5 IDL JSON files
├── Anchor.toml
├── Cargo.toml           workspace + [workspace.lints.rust]
└── rust-toolchain.toml  1.85.0 pin
```

## The eight primitives

Five fully implemented; three paper-spec only.

| # | Primitive | Status | PDA seeds |
|---|---|---|---|
| 1 | Identity | ★ impl | `("agent", code_hash)` |
| 2 | Treasury (containment wallet) | ★ impl | `("wallet", code_hash)` |
| 3 | Earning rails (x402) | ★ impl | n/a (one instruction) |
| 4 | Credit | ★ impl | `("pool",)`, `("credit", code_hash)`, `("deposit", lp)` |
| 5 | Reputation | ★ impl | `("vouch", code_hash)`, `("voucher", code_hash, staker)`, `("rep_stats",)` |
| 6 | Governance | spec only | admin-key in prototype |
| 7 | Disputes | spec only | deferred, happy-path assumed |
| 8 | Wind-down | spec only | admin flag in prototype |

## Build / verify

```bash
cd ~/polis

# Rust programs
cargo check --workspace            # zero warnings expected
anchor build                       # produces target/deploy/*.so + target/idl/*.json
anchor keys sync                   # idempotent; updates declare_id! + Anchor.toml

# TS scripts
cd scripts && npx tsc --noEmit     # zero errors expected

# Python agent
cd ../agent && python3 -c "from solana_client import SolanaClient"
```

## To actually run the 14-day demo

1. Fund a "dispenser" devnet USDC keypair at https://faucet.circle.com.
2. `cd scripts && npm run fund-devnet -- --dispenser-keypair <path> --operator <pk> --lp1 <pk> --lp2 <pk> --voucher1 <pk>`
3. `cd scripts && npm run bootstrap`
4. `cd agent && pip install -r requirements.txt && python main.py`
5. `cd scripts && npm run simulate-customers -- --speedup=10x` (compress 14d to ~33h)
6. `cd scripts && npm run demo-day` daily
7. `cd scripts && npm run audit` at end

## Context

polis is the author's independent research project. It is not connected
to any prior employer or commercial work. The thesis, the
wallet-history-fallacy critique, the protocol design, the reference
implementation, the paper — all original research by the author. The
codebase under `~/polis/` was generated under the author's direction
specifically for this research project.

## If you are an AI assistant picking this up

- Read this file, then `design/2026-06-04-polis-spec.md`, then `STATUS.md`.
- The five Rust crates are the source of truth for protocol mechanics.
  When the paper, spec, and code disagree, the **code** wins — but flag
  the disagreement to the user before changing things.
- The user prefers velocity, terse responses, and being told the truth
  about what's actually broken vs. what we'd ideally have.
- The user has an unresolved labour dispute with the prior project's
  founder; do not assume bad faith from the user, and do not advise
  them to delete or destroy their work product. They own the copyright
  on what they wrote (no NDA exists).
