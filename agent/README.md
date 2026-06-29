# polis reference agent

A Python implementation of the reference agent specified in
`design/2026-06-04-polis-spec.md`, §"Reference agent". It demonstrates the
14-day self-sustaining loop the polis thesis defends.

## What it does

The agent sells one service — LLM summarization — via a FastAPI
`POST /summarize` endpoint. For each call:

1. The customer pays the agent's `Wallet` PDA via the polis `x402` program's
   `pay_for_service` instruction, including a `payment_id` as the on-chain memo.
2. The agent observes the resulting `PaymentReceived` event over WebSocket and
   caches the `payment_id`.
3. When the customer posts to `/summarize` with the same `payment_id`, the
   agent verifies the cached payment, runs Claude on the input, and returns
   the summary.

Concurrently, the agent:

- Polls its `Wallet` PDA balance every 10 seconds.
- Auto-borrows $10 USDC from the polis `credit` Pool when the balance drops
  below the threshold (default $5).
- Auto-repays daily, using a configurable fraction (default 50%) of
  yesterday's earnings.
- Auto-accrues interest at `accrue` intervals so the line stays up to date.

No human signs any transaction during the run other than LP deposits and
customer payments (spec §"Pass criteria").

## File layout

| File | Purpose |
|------|---------|
| `server.py` | FastAPI app: `/summarize`, `/pricing`, `/status`, `/code-hash`, `/health` |
| `main.py` | Orchestrator: env load, registration verify, server + polling loop |
| `treasury_manager.py` | Wallet PDA balance polling + low-balance signal |
| `credit_manager.py` | Auto-borrow / auto-repay policy against the credit program |
| `solana_client.py` | Anchor-style ix encoding, PDA derivation, payment subscriber |
| `llm_client.py` | Anthropic SDK wrapper, token accounting, cost estimate |
| `code_hash.py` | Blake3 hash over `agent/*.py` — the agent's on-chain identity |
| `requirements.txt` | Python deps |
| `.env.example` | Required environment variables |

## Run it

```bash
cd agent
python3.11 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt

cp .env.example .env
# fill in OPERATOR_KEYPAIR_PATH, ANTHROPIC_API_KEY, and the 5 POLIS_*
# program IDs after deploying to devnet.

python main.py
```

The server listens on `HTTP_HOST:HTTP_PORT` (default `0.0.0.0:8000`).

## Registration (one-time)

The agent's identity is **its code-hash**, not its wallet. Before the agent
can be paid or borrow, the operator must register it:

1. Get the agent's code-hash:
   ```bash
   python code_hash.py
   ```
2. Run the bootstrap script (TypeScript; lives in `~/polis/scripts/`):
   ```bash
   ts-node ../scripts/bootstrap.ts --code-hash <hex> --operator <keypair-path>
   ```
   This calls `identity.register_agent(code_hash, operator_key)` and posts the
   stake bond (default $50 USDC) per spec §"Identity layer / Registration".

After registration the agent's source files **must not change** — any edit
invalidates the code-hash and breaks identity. To upgrade, register a new
identity under a new code-hash.

### Dev mode

For development, set `DEV_MODE=true` in `.env`. This skips the on-chain
payment check on `/summarize` and lets you exercise the LLM path without a
fully-deployed protocol. The agent logs a warning on each call. **Never run
DEV_MODE in production.**

## Endpoints

| Method & Path | Description |
|--------------|-------------|
| `POST /summarize` | body: `{ "text": "...", "payment_id": "..." }` → `{ "summary": "...", "cost_estimate_usd": 0.0001 }`. Returns 402 + `Payment-Required` header if payment not observed. |
| `GET /pricing` | Returns price per call, wallet PDA, mint, x402 program. |
| `GET /status` | Wallet balance, credit position, reputation, earnings. |
| `GET /code-hash` | Hex-encoded blake3 of `agent/*.py`. Customers can verify this matches the on-chain `AgentProfile.code_hash`. |
| `GET /health` | Liveness. |

## Pass criteria (spec §"Reference agent / Pass criteria")

The 14-day demo is a success iff:

1. The agent runs uninterrupted for 14 days.
2. At least 2 complete credit cycles (draw → repay) happen without operator
   intervention.
3. Reputation score is monotonically non-decreasing across the run.
4. The wallet ends with ≥ starting balance after fees and compute costs.
5. No human signs any transaction during the run other than LP deposits and
   customer payments.

These conditions are checked by `scripts/audit.ts` (lives in the polis repo,
not in `agent/`).

## Status & known limitations

- All five on-chain polis programs (`identity`, `wallet`, `x402`, `credit`,
  `reputation`) are deployed. The agent talks to them via `anchorpy`,
  loading IDLs from `../target/idl/` (override path with `$IDL_PATH`).
  See `idls.py` for the loader (which adapts Anchor 0.30 IDLs to the
  schema understood by `anchorpy 0.20.1`).
- Reputation scoring reads `RepStats.max_total_staked_seen` directly —
  no `getProgramAccounts` sweep needed. If `RepStats` is not yet
  initialised the read falls back to "max == own stake".
- Payment event parsing flows through `anchorpy.EventParser`, which decodes
  the x402 program's `PaymentReceived` event from `Program data:` logs.
  The `memo: Option<String>` field is treated as the off-chain `payment_id`
  (spec open question #2).
