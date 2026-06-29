# polis scripts

Operational tooling for the 14-day demo described in `design/2026-06-04-polis-spec.md`.
These four TypeScript scripts implement the off-chain side of the existence
proof: they set up the chain state, generate customer traffic, take daily
snapshots, and verify pass-criteria at the end of the run.

```
scripts/
├── _common.ts                 # shared utilities (config, PDA derivation, keypair loading)
├── bootstrap.ts               # one-time devnet setup
├── simulate-customers.ts      # long-running customer traffic generator
├── demo-day.ts                # daily snapshot cron
├── audit.ts                   # end-of-demo pass/fail report
├── logs/                      # CSVs + daily snapshots + audit-report.md
└── README.md
```

## Prerequisites

1. **Anchor build + deploy.** All five primitive programs must be deployed to
   devnet. Take the resulting program IDs and place them in `../.anchor-config.json`
   (template below). Until that file exists, every script will fail fast.
2. **Funded keypairs.** Admin, two LPs, the agent operator, an initial voucher,
   and however many customer wallets you want for the simulator. Each must have
   USDC on devnet (Circle's faucet).
3. **Node 20+** with `yarn install`.
4. **A `.env` file** at the polis repo root with the keypair paths and (optionally)
   `SOLANA_RPC_URL`. See the env vars enumerated in each script's docstring.

### `.anchor-config.json` template

```json
{
  "cluster": "devnet",
  "rpcUrl": "https://api.devnet.solana.com",
  "usdcMint": "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU",
  "adminPubkey": "<admin pubkey>",
  "programs": {
    "identity":   "<post-deploy program id>",
    "wallet":     "<post-deploy program id>",
    "x402":       "<post-deploy program id>",
    "credit":     "<post-deploy program id>",
    "reputation": "<post-deploy program id>"
  }
}
```

`bootstrap.ts` extends this file with derived PDAs after the first run.

## Scripts

### `bootstrap.ts`

```bash
yarn bootstrap --code-hash-file=../agent/code-hash.txt
```

One-shot devnet setup. Idempotent — already-initialised accounts are detected
and skipped, so it's safe to re-run mid-troubleshooting.

Steps:
1. Loads admin / LP1 / LP2 / operator / voucher keypairs from env.
2. Derives all PDAs from the deploy-time `.anchor-config.json` + the agent's
   code-hash.
3. Initialises `PolisConfig`, the credit `Pool`, and its USDC ATA.
4. Seeds `Pool` with $1,000 USDC ($600 LP1 + $400 LP2).
5. Registers the demo agent via `identity::register_agent`, posting a $50
   stake bond.
6. Initialises the agent's `Wallet` PDA + USDC ATA.
7. Has the initial voucher stake $10 against the agent.
8. Prints all PDAs, balances, and a copy-pasteable agent `.env` block.
9. Writes the derived addresses back into `.anchor-config.json` so subsequent
   scripts don't have to re-derive.

> **Note:** the credit, reputation, and x402 programs were stubs at the time
> these scripts were written. `bootstrap.ts` issues `[deferred]` markers and
> falls back to direct SPL transfers where necessary so the demo can begin
> with realistic balances. When the IDLs land, swap the `[deferred]` paths
> for the real Anchor `methods.<ix>(...)` calls.

### `simulate-customers.ts`

```bash
yarn simulate-customers --agent-url=http://127.0.0.1:8000 --speedup=1 --rate=5
```

Long-running script. Poisson-distributed inter-arrival times with mean
`86_400 / rate / speedup` seconds. For each call:

1. Generates a random 32-byte payment_id.
2. Picks a random customer keypair.
3. Submits an x402-style payment ($0.10 default) to the agent's wallet ATA.
4. POSTs `{text, payment_id}` to `<agent-url>/summarize`.
5. Logs the outcome (timestamp, customer, payment_id, sig, status, latency,
   error) to `logs/customer-traffic.csv`.

`--speedup=10` packs ~50 calls/day into the simulation in real time, useful
for local smoke-testing without waiting the full 14 days.

### `demo-day.ts`

```bash
yarn demo-day                       # snapshot today
yarn demo-day --date=2026-06-08     # snapshot a specific UTC date
```

Daily snapshotter. Polls the relevant PDAs and writes a JSON file to
`logs/daily-snapshots/YYYY-MM-DD.json`. Includes:

- Agent `is_retired` flag.
- Wallet / StakeBond / Pool / VouchPool USDC balances.
- Wallet `is_paused`, `per_trade_cap_bps`, `total_inflow`, `total_outflow`
  (parsed directly from the wallet account's borsh-ish byte layout — see the
  `parseWalletAccount` helper).
- Today's calls + earnings from `logs/customer-traffic.csv`.
- Cumulative calls + earnings.

Run via cron at 00:00 UTC.

### `audit.ts`

```bash
yarn audit
```

End-of-demo verification per spec §"Pass criteria for the existence proof".

Findings:
- **C1** Continuity — a snapshot exists for each of the 14 days.
- **C2** ≥ 2 complete credit cycles (draw → repay) — read from
  `logs/credit-events.json` if present; otherwise heuristically inferred
  from Pool balance dips + recoveries in the daily snapshots.
- **C3** Reputation monotonicity — total vouched amount is non-decreasing.
- **C4** Wallet ends with ≥ starting balance.
- **C5** No-human-signing — pulls every signature touching a polis PDA in
  the 14-day window and verifies the only signers were:
   - LPs / customers (allowlisted via `allowed_human_signers.json`), or
   - The agent operator key, signing one of `{borrow, repay, accrue,
     transfer_out, record_inflow}`.

Writes `logs/audit-report.md`. Exits non-zero on any failure.

## 14-day demo workflow

| Day | Action |
|---|---|
| –1 | `anchor build && anchor deploy`; populate `.anchor-config.json` with the new program IDs. |
| 0  | `yarn bootstrap …`; copy the printed agent `.env`; start the agent + customer simulator. |
| 0–13 | Cron `yarn demo-day` daily at 00:00 UTC. |
| 14 | `yarn audit`. Commit the resulting `audit-report.md` to the paper repo as a citation. |

## Typecheck

```bash
yarn typecheck   # tsc --noEmit
```

Should pass with zero errors against the deps in `package.json`.
