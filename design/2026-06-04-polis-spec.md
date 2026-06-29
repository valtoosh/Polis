# polis — Design Spec

**Date:** 2026-06-04
**Author:** Author Name (TBD)
**Status:** Approved (brainstorm), pending implementation plan

---

## Context

### Thesis

> **AI agents will become self-sustaining earners without a human in the loop, with the help of DeFi.**

This work defends that thesis with:
1. A position paper articulating *why* DeFi specifically (vs. traditional banking or Web2 marketplaces) is the right substrate;
2. A protocol specification — **polis** — supplying the eight primitives an autonomous agent needs to operate as a citizen of a self-governing economic polity;
3. A working reference implementation on Solana with a reference agent that demonstrates a self-sustaining 14-day loop without human intervention.

### Why a polity, not a bank

A bank supplies one primitive (credit / custody). The thesis requires the full institutional stack: identity, banking, credit, reputation, governance, dispute resolution, exit. The smallest historical unit of organization that supplies these as a coherent set is a *polis* — the Greek city-state. We rebuild that institutional pattern as a protocol.

### The wallet-history fallacy (keystone insight)

Existing on-chain credit / reputation systems (Aave score gating, ARCx, Cred Protocol, the Krexit Score model in prior work by this author) bind reputation to wallet history. This is **fundamentally Sybil-vulnerable**, not in the usual "many small accounts" sense, but in a sharper way:

> **An agent is not its wallet.** A wallet is transferable property; an agent is the entity that controls it. An adversary can assign a fresh agent a wallet with a year of clean on-chain history, and any wallet-history-based score returns "trustworthy." Reputation is detachable from the actor that earned it.

polis solves this by binding identity (and therefore reputation, credit, and everything downstream) to a **code-hash + attestation primitive**, not to a wallet. Identity attaches to what the agent **is**, not what it **has**.

---

## Goals

### Primary goal — thesis defense

Demonstrate, by existence proof, that:
- One reference agent can earn (via x402-style settlement), spend (on its own compute / API costs), bank (via a containment wallet), borrow (against reputation), repay (atomically from earnings), and operate **uninterrupted for 14 days with no human signing any non-LP / non-customer transaction**.

### Secondary goal — foundation for other builders

The protocol must be a *foundation*, not a finished product. Other builders should be able to:
- Spin up new agents that plug into polis without protocol changes.
- Build new services on top (specialized credit, dispute backends, identity providers).
- Fork the reference agent and replace its earning logic with their own.

### Non-goals (explicitly out of scope for the prototype)

- Multi-asset support (USDC only for the prototype; paper mentions extensibility).
- Multi-chain support (Solana only; paper notes Base / EVM as a portability discussion).
- Production-grade TEE attestation (prototype uses `code_hash + stake bond`; paper specifies TEE as the production primitive).
- Full DAO governance (prototype uses an admin key; paper specifies bicameral DAO).
- Dispute resolution implementation (prototype assumes happy path; paper specifies the protocol).
- Sybil resistance beyond what stake-bonding provides.
- Mechanised formal verification (proof sketches only).

---

## The eight primitives

Four layers, eight primitives. Five are fully implemented in the prototype (★); three are paper-specified but mocked / deferred in the prototype (☆).

### Identity layer

**1. Identity ★**

- **PDA:** `("agent", code_hash)` → `AgentProfile`
- **Fields:** `code_hash: [u8; 32]`, `operator_key: Pubkey`, `created_at: i64`, `is_retired: bool`
- **Registration:** operator calls `register_agent(code_hash, operator_key)` once; one identity per code-hash. Operator posts a stake bond (USDC, configurable; default $50) into a `StakeBond` PDA.
- **Verification:** any caller can challenge the live agent at the agent's HTTP endpoint with an input; the agent returns a deterministic signed response. Anyone can compare against the registered `code_hash`.
- **Paper note:** TEE attestation is the production primitive. Verification then resolves to checking the TEE quote against an on-chain DCAP verifier. The prototype's stake-bond replaces the TEE's economic guarantee with a slashable bond.
- **Property P-Identity:** identity binds to `code_hash`, not to any wallet. Re-registering a fresh agent under the same operator produces a different identity. Reputation cannot transfer between identities.

### Capital layer

**2. Treasury (containment wallet) ★**

- **PDA:** `("wallet", code_hash)` → `Wallet`
- **Fields:** `usdc_balance` (derived from PDA token account), `is_paused: bool`, `per_trade_cap_bps: u16`, `daily_volume_log` (paper-only; not in prototype)
- **Outbound transfer safety:** two layers in prototype:
  1. `assert !config.is_paused`
  2. `assert amount <= per_trade_cap_bps * balance / 10000` (default 2,000 bps = 20%)
- **Paper note:** production adds six more layers (daily limit, venue exposure cap, health-factor gate, venue whitelist, frozen flag, liquidation flag).
- **Property P-Containment:** the agent's `operator_key` never signs for an outbound transfer from `Wallet`. The PDA can only be signed by the wallet program with seeds `("wallet", code_hash)`, after the two-layer guard passes.

**3. Earning rails (x402) ★**

- **One instruction:** `pay_for_service(agent: Pubkey, amount: u64, memo: Option<String>)`
- Transfers `amount` USDC from caller to `Wallet` PDA; emits `PaymentReceived(agent, payer, amount, memo, timestamp)`.
- **Paper note:** production wraps this in the full HTTP-402 wire protocol (`Payment-Required` header, `X-Payment` retry, multi-mint `accepts[]` schema). Prototype skips the HTTP ceremony and exercises only the on-chain settlement primitive.

**4. Credit (single-pool) ★**

- **PDAs:** `Pool` (singleton), `CreditLine` per agent (`("credit", code_hash)`).
- **Pool fields:** `total_deposits`, `total_shares`, `total_drawn`, `total_interest_accrued`.
- **CreditLine fields:** `agent: Pubkey`, `principal: u64`, `accrued_interest: u64`, `apr_bps: u16`, `last_accrual: i64`, `is_active: bool`.
- **Deposit:** LP calls `deposit(amount)`; receives shares = `amount * total_shares / total_deposits` (first deposit: shares = amount).
- **Borrow:** agent calls `borrow(amount)`; protocol checks `amount <= credit_limit(reputation(agent))` and `amount <= pool.available`. Transfers USDC to `Wallet`; opens or updates `CreditLine`.
- **Interest accrual:** simple interest, per-second granularity:
  ```
  ΔI = (principal * apr_bps * Δt) / (10_000 * 31_536_000)
  ```
  Invoked at every state transition on the credit line.
- **Repay:** transfers USDC from `Wallet` to `Pool`. Interest is paid first (with a 30% slice routed to vouchers; see Reputation), then principal.
- **Default:** if a `CreditLine` exceeds `default_after_seconds` (configurable; default 30 days unpaid) the protocol marks it defaulted. The defaulted amount is:
  1. Recovered from the agent's `VouchPool` (50% to Pool, 50% burned)
  2. Remaining loss distributed pro-rata across LP shares
- **Property P-Credit:** all losses are accounted for. No silent debt forgiveness.

### Social layer

**5. Reputation ★**

- **PDA:** `("vouch", code_hash)` → `VouchPool`
- **Fields:** `total_staked: u64`, `vouchers: Map<staker_pubkey, Voucher>`; `Voucher { amount: u64, stake_time: i64, cooldown_started: Option<i64> }`.
- **Vouch:** `vouch(agent: Pubkey, amount: u64)` — transfers USDC from staker to VouchPool, records `Voucher`.
- **Unvouch:** `request_unvouch(agent: Pubkey)` starts a 7-day cooldown; `withdraw_vouch(agent: Pubkey)` after cooldown returns the stake.
- **Voucher yield:** on every credit repayment, 30% of the interest paid is routed to the VouchPool pro-rata (paid from the Credit primitive's repay flow).
- **Slashing:** on agent default, the slashing module called from Credit drains the VouchPool: 50% → Pool, 50% → burn address.
- **Reputation score:**
  ```
  rep(agent) = total_staked(agent) / max_staked_across_all_agents
  ```
  A simple ratio in [0, 1].
- **Credit-reputation coupling (prototype hardcoded):**
  | `rep(a)` | Max credit | APR (bps) |
  |---|---|---|
  | < 0.01 | 0 | — |
  | 0.01 – 0.05 | $500 | 3,650 |
  | 0.05 – 0.20 | $5,000 | 2,400 |
  | 0.20 – 0.50 | $50,000 | 1,800 |
  | ≥ 0.50 | $500,000 | 1,200 |
- **Property P-Reputation:** vouchers have economic skin in the game. Slashing is automatic on default; no human curates the reputation set.

### Civic layer

**6. Governance ☆ (paper-spec, hardcoded in prototype)**

- **Production design:** bicameral DAO.
  - **Stake House:** 1 vote per USDC in any pool (LP Pool, any VouchPool). Initiates parameter-change proposals.
  - **Citizen Assembly:** 1 vote per attested identity. Acts as a veto. Anti-plutocracy check.
  - Proposals pass if both chambers approve within one epoch.
  - Controls: APR table, credit-level thresholds, slash percentages, voucher yield share, protocol fees.
- **Prototype:** `PolisConfig` PDA with an `admin` key controls all governance-set parameters. Admin can update tables; no voting.
- **Documented limitation:** "Admin-only governance contradicts the no-human-in-the-loop thesis. Production replaces this with the bicameral DAO. Specified in §Governance of the paper; not implemented for the prototype."

**7. Disputes ☆ (paper-spec, deferred in prototype)**

- **Production design:** stake-bonded jury arbitration.
  - Any citizen files a dispute by bonding 1% of disputed amount.
  - Random sample of N citizens, drawn proportionally to reputation, are conscripted as jurors.
  - Jurors review on-chain evidence + agent's signed outputs; vote yes/no within deadline.
  - Majority wins; losing side's bondholders are slashed; jurors who voted with majority earn fees.
  - Inspired by Kleros and classical Athenian dikastēria.
- **Prototype:** deferred entirely. Reference agent assumes happy path. Paper specifies; prototype assumes.

**8. Wind-down ☆ (paper-spec, mocked in prototype)**

- **Production design:** `initiate_wind_down(agent)` callable by operator-of-record.
  - Marks agent withdrawal-only.
  - Routes 100% of incoming `pay_for_service` calls to debt repayment until LP Pool repaid.
  - Releases vouchers after a 30-day clawback window.
  - Distributes residual treasury to a successor address (specified at registration) or to a public-goods fund.
  - Burns the identity; re-registering the same `code_hash` requires a fresh cycle.
- **Prototype:** `admin` can set `is_retired = true`. No automated settlement. Documented.

---

## Reference agent

A Python process implementing a self-sustaining loop. Sells **LLM summarization** via `/summarize`.

### Code layout

```
agent/
├── server.py             # Flask / FastAPI HTTP server
├── treasury_manager.py   # tracks Wallet balance, signals when low
├── credit_manager.py     # auto-borrows + auto-repays
├── solana_client.py      # signs and submits txs
├── llm_client.py         # wraps Claude API
└── README.md             # how to run
```

### Loop logic (pseudo)

```python
on startup:
    register identity (one-time, by operator CLI)
    load operator stake bond

every 10s:
    balance = wallet.balance()
    rep    = reputation.score(agent_pubkey)
    if balance < $5 and credit.active_debt() < credit.limit(rep):
        credit.draw($10)
    if rep > 0 and time_since_last_repay > 24h and credit.active_debt() > 0:
        credit.repay(min(balance * 0.5, credit.active_debt()))

on POST /summarize:
    txt = request.body
    assert payment_received_for(request_id)
    summary, cost_usd = llm.summarize(txt)
    wallet.transfer(llm_provider, cost_usd)
    return summary
```

### Demo parameters

- **Duration:** 14 days.
- **Bootstrapping:** operator stakes $50; Pool seeded with $1,000 by 2 LPs; 1 initial voucher stakes $10.
- **Customers:** simulated via `simulate-customers.ts`. Poisson rate ≈ 5 calls/day. Mix of paying and self-funded calls.
- **Pricing:** $0.10 per `/summarize`; underlying Claude cost ≈ $0.02.

### Pass criteria for the existence proof

1. Agent runs uninterrupted for 14 days.
2. At least 2 complete credit cycles (draw → repay) without operator intervention.
3. Reputation score is monotonically non-decreasing across the run.
4. Wallet ends with ≥ starting balance after fees and compute costs.
5. No human signs any transaction during the run other than LP deposits and customer payments.

---

## Paper outline

**Target:** ~30 pages, position paper + reference architecture style. Single column, 11pt.

| § | Section | Pages |
|---|---|---|
| 1 | Introduction — agent-as-economic-actor thesis | 2 |
| 2 | The wallet-history fallacy | 1 |
| 3 | Background: Solana, TEE attestation, x402 | 1 |
| 4 | Related work | 2 |
| 5 | Threat model & design goals | 2 |
| 6 | Architecture: the polis | 2 |
| 7 | Identity primitive | 2 |
| 8 | Treasury primitive | 1 |
| 9 | Earning rails (x402) | 1 |
| 10 | Credit primitive | 2 |
| 11 | Reputation primitive | 2 |
| 12 | Governance primitive | 2 |
| 13 | Disputes primitive | 1 |
| 14 | Wind-down primitive | 1 |
| 15 | Reference agent + 14-day demo | 3 |
| 16 | Formal properties & proof sketches | 2 |
| 17 | Discussion, limitations, open problems | 1 |
| 18 | Conclusion | 0.5 |
| Appx A | Algorithm pseudocode | 1 |
| Appx B | Code listings | 1 |
| Appx C | x402 wire format | 0.5 |

---

## Threat model & properties

### Adversary

- Standard permissionless model: any wallet other than `admin` is potentially adversarial.
- Operator may be malicious.
- Operator may deploy multiple agents (Sybil); cost is the stake bond per identity.
- Adversary may try to vouch + immediately default to drain LP Pool (mitigated by 7-day vouch cooldown + slashing).

### Properties

| ID | Statement | Hardened via |
|---|---|---|
| P-Identity | Identity binds to `code_hash`, not wallet | PDA seed = `code_hash` |
| P-Containment | Funds in Wallet PDA exit only through 2-layer guard | `invoke_signed` paths reviewed |
| P-Credit | All defaults flow through Reputation slashing → LP haircut; no silent loss | Default code path single, atomic |
| P-Reputation | Reputation is identity-bound, slashable, market-priced | VouchPool PDA = `code_hash` |
| P-No-Human | Reference agent's 14-day run has zero human transactions outside LP / customers | Demo audit log + chain inspection |

---

## Folder layout (target)

```
~/polis/
├── README.md
├── paper/
│   ├── polis.tex
│   └── polis.md
├── design/
│   └── 2026-06-04-polis-spec.md   <- this document
├── programs/
│   ├── identity/
│   ├── wallet/
│   ├── x402/
│   ├── credit/
│   └── reputation/
├── agent/
│   ├── server.py
│   ├── treasury_manager.py
│   ├── credit_manager.py
│   ├── solana_client.py
│   ├── llm_client.py
│   └── README.md
└── scripts/
    ├── bootstrap.ts
    ├── simulate-customers.ts
    ├── demo-day.ts
    └── audit.ts                  <- verifies pass criteria
```

---

## Delivery plan

| Phase | Output | Time |
|---|---|---|
| 0 | This spec, approved | done |
| 1 | Anchor program scaffolding for 5 primitives | 3 days |
| 2 | Identity + Wallet programs working on devnet | 3 days |
| 3 | x402 + Credit programs working on devnet | 4 days |
| 4 | Reputation program (most novel; needs careful design) | 4 days |
| 5 | Reference agent (Python) + LLM wrapper | 3 days |
| 6 | Customer simulator + bootstrap scripts | 2 days |
| 7 | Run 14-day demo, collect telemetry | 14 days (calendar; ~1 day of work to monitor) |
| 8 | Paper draft, finalised with demo data | parallel to phase 7 |

Total focused work: **~3 weeks** of build + 14 days of calendar time for the demo + paper writing in parallel.

---

## Open questions

1. **Code-hash determinism.** How do we hash a Python agent's source so that a customer can verify it matches? Open: hash the venv'd dependencies + `agent/*.py` files? Pin Python version? May need to provide a Docker image as the canonical artifact and hash that.
2. **Customer-side proof of payment.** Customer pays first, calls `/summarize` second. How does the agent know which `PaymentReceived` event corresponds to which request? Likely: customer includes the request hash as the memo on the payment.
3. **LLM cost variance.** Claude / OpenAI costs are per-token, variable per request. How does the agent price its service against unknown future costs? Likely: short rolling-window forecast + small markup; surface variance as a research question in the paper.
4. **Substrate-agnostic future work.** The paper claims the design is substrate-agnostic, but the only reference impl is Solana. We should write at minimum a one-page Base/EVM portability sketch to support the claim.
5. **Where does the reference agent run?** Suggestion: a small VPS the author controls. The thesis specifies "without a human in the loop" *operationally*, not "without humans existing"; an operator paying for a VPS is fine. The paper should make this distinction explicit.

---

*End of design spec.*
