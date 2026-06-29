# polis

> **polis** is an on-chain protocol providing the full institutional stack — identity, banking, credit, reputation, governance, dispute resolution — that an AI agent needs to operate as a citizen of a self-governing economic polity without a human in the loop.

A bank supplies one primitive. An autonomous agent needs the whole institutional pattern: identity, treasury, earning rails, credit, reputation, governance, disputes, and a managed exit. The smallest historical unit of organization that supplies these as a coherent set is a *polis* — the Greek city-state. polis rebuilds that pattern as a Solana protocol.

The keystone insight: **an agent is not its wallet.** Existing on-chain credit and reputation systems bind reputation to wallet history; wallets are transferable property, so reputation can be detached from the actor that earned it. polis binds identity (and therefore everything downstream) to a code-hash + stake-bond primitive, not to a wallet.

For the full thesis, threat model, and protocol definition, see [`design/2026-06-04-polis-spec.md`](design/2026-06-04-polis-spec.md).

## Research paper

- **Title:** *polis: A Programmable Polity for Self-Sustaining Autonomous AI Agents*
- **Authors:** Raj Singh, Dr. A.R. ABDUL RAJAK, Dr. Sunil Thomas, Dr. Raja Muthalagu
- **LaTeX source:** [`paper/polis.tex`](paper/polis.tex)
- **Paper notes:** [`paper/README.md`](paper/README.md)

Compile locally (from `paper/`):

```bash
pdflatex polis.tex
pdflatex polis.tex
```

---

## Architecture: the eight primitives

Four layers, eight primitives. **Five** are implemented as on-chain Anchor programs in this repository (★); **three** are paper-specified but mocked or deferred in the prototype (☆).

| # | Layer | Primitive | Status | Location |
|---|---|---|---|---|
| 1 | Identity | **Identity** | ★ implemented | `programs/identity/` |
| 2 | Capital | **Treasury** (containment wallet) | ★ implemented | `programs/wallet/` |
| 3 | Capital | **Earning rails** (x402) | ★ implemented | `programs/x402/` |
| 4 | Capital | **Credit** (single-pool) | ★ implemented | `programs/credit/` |
| 5 | Social | **Reputation** (vouching + slashing) | ★ implemented | `programs/reputation/` |
| 6 | Civic | **Governance** (bicameral DAO) | ☆ paper-only; admin key in prototype | paper §Governance |
| 7 | Civic | **Disputes** (stake-bonded jury) | ☆ paper-only | paper §Disputes |
| 8 | Civic | **Wind-down** | ☆ mocked (`retire_agent` only) | partial in `programs/identity/` |

The prototype's purpose is to demonstrate, by existence proof, that a single reference agent can earn, spend, bank, borrow, and repay uninterrupted for 14 days with no human signing any non-LP / non-customer transaction.

---

## Folder layout

```
polis/
├── Cargo.toml                # Rust workspace manifest (5 program members)
├── Anchor.toml               # Anchor workspace config (devnet)
├── README.md                 # this file
├── design/
│   └── 2026-06-04-polis-spec.md   # full design spec
├── paper/                    # IEEE-style manuscript sources
├── programs/                 # on-chain Anchor programs
│   ├── identity/             # ★ identity primitive (code_hash PDA + stake bond)
│   ├── wallet/               # ★ treasury / containment wallet
│   ├── x402/                 # ★ earning rails (USDC settlement)
│   ├── credit/               # ★ single LP pool + per-agent credit lines
│   └── reputation/           # ★ vouching pool + slashing
├── agent/                    # Python reference agent
│   ├── server.py             # Flask/FastAPI HTTP server (/summarize)
│   ├── treasury_manager.py   # wallet balance tracker
│   ├── credit_manager.py     # auto-borrow / auto-repay loop
│   ├── solana_client.py      # tx signing & submission
│   ├── llm_client.py         # Claude API wrapper
│   └── requirements.txt      # Python dependencies
└── scripts/                  # operator / demo automation
    ├── bootstrap.ts          # devnet setup (mint USDC, seed Pool, deploy agent)
    ├── simulate-customers.ts # Poisson-process customer simulator
    ├── demo-day.ts           # 24h slice of the 14-day demo
    └── audit.ts              # verifies the pass criteria
```

---

## Build

### Prerequisites

- Rust toolchain (1.82+) — `rustup install stable`
- Solana CLI (1.18+) — see [solana.com/docs/install](https://solana.com/docs/install)
- Anchor (0.30.1) — `cargo install --git https://github.com/coral-xyz/anchor avm --locked --force && avm install 0.30.1 && avm use 0.30.1`
- Node 18+ / Yarn for scripts
- Python 3.11+ for the reference agent

### Build the on-chain programs

```bash
anchor build
```

After the first build, sync program IDs (the workspace ships with placeholder system-program IDs that must be replaced before deploy):

```bash
anchor keys sync
anchor build
```

To compile a single program without going through Anchor's wrapper:

```bash
cargo check -p identity
```

### Build the reference agent

```bash
cd agent
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
```

---

## Run

The end-to-end devnet bring-up is automated:

```bash
# 1. Configure Solana CLI to devnet, fund a keypair
solana config set --url devnet
solana airdrop 5

# 2. Deploy programs (after `anchor keys sync` + `anchor build`)
anchor deploy --provider.cluster devnet

# 3. Bootstrap: mint mock USDC, seed LP Pool, register reference agent
yarn ts-node scripts/bootstrap.ts

# 4. Start the reference agent + customer simulator
python agent/server.py &
yarn ts-node scripts/simulate-customers.ts &

# 5. Verify the demo against the pass criteria
yarn ts-node scripts/audit.ts
```

See [`design/2026-06-04-polis-spec.md`](design/2026-06-04-polis-spec.md) §"Reference agent" for the demo parameters and pass criteria.

### USDC mint addresses

The Identity, Wallet, x402, Credit, and Reputation programs all accept the USDC mint as an instruction-context account so the workspace is portable across clusters. Use the canonical mint for your target:

| Cluster | Mint |
|---|---|
| Mainnet-Beta | `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` |
| Devnet (USDC-Dev) | `4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU` |

In bootstrap scripts on devnet you may also mint your own SPL token with 6 decimals; the program is mint-agnostic and verifies only that the source and destination ATAs use the same mint.

---

## Implementation status (snapshot)

The five Anchor programs and one Python reference agent are the deliverables of the prototype. Of those:

- **`identity`** — full implementation. Registration creates an `AgentProfile` PDA keyed by `code_hash`, escrows a USDC stake bond into a `StakeBond` PDA-owned ATA, and emits `AgentRegistered`. Withdrawal is operator-gated and retirement-gated.
- **`wallet`, `x402`, `credit`, `reputation`** — see the per-program `src/lib.rs` for instruction-level status.

The civic-layer primitives (governance, disputes, wind-down) are paper-specified only. Their absence from the prototype is documented in the design spec.

---

## License

Research code.

- Paper: **CC-BY-4.0**
- Code: **Apache-2.0**
