"""
run_walkthrough.py  —  polis single functional walkthrough (localnet)

Drives the reference agent through every economic primitive once, with real
on-chain transactions and real NVIDIA-hosted LLM inference, and records the
resulting numbers to scripts/logs/walkthrough-results.json for the paper.

Lives at repo root (NOT in agent/) so it does not perturb the agent code-hash.
Run with the agent venv:  agent/.venv/bin/python run_walkthrough.py
"""
from __future__ import annotations

import asyncio
import json
import os
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
AGENT = ROOT / "agent"
sys.path.insert(0, str(AGENT))

from dotenv import load_dotenv  # noqa: E402

load_dotenv(AGENT / ".env")

from solders.pubkey import Pubkey  # noqa: E402
from code_hash import compute_code_hash  # noqa: E402
from solana_client import (  # noqa: E402
    SolanaClient,
    ProgramIds,
    derive_ata,
    usd_to_base,
    base_to_usd,
)
from llm_client import LLMClient  # noqa: E402

RPC = os.environ["SOLANA_RPC_URL"]
MINT = Pubkey.from_string(os.environ["SOLANA_USDC_MINT"])
OPERATOR_KP = os.environ["OPERATOR_KEYPAIR_PATH"]
CUSTOMER_KP = "/tmp/polis-keys/customer1.json"

SERVICE_PRICE = usd_to_base(0.10)
BORROW_AMT = usd_to_base(10.0)
N_CALLS = 8

SAMPLE_TEXTS = [
    "Decentralized finance protocols enable lending, borrowing, and trading "
    "without intermediaries by encoding the rules in smart contracts on a "
    "public blockchain. Liquidity providers deposit assets into pools and earn "
    "yield from the fees paid by borrowers and traders.",
    "An autonomous software agent that controls its own wallet can pay for its "
    "compute, accept payment for services it renders, and borrow against future "
    "earnings, all without a human approving each transaction.",
    "Reputation that is bound to a transferable wallet can be bought: an "
    "adversary acquires a wallet with a long clean history and inherits its "
    "score. Binding reputation to the agent's code hash defeats this attack.",
]


def now_ms() -> int:
    return int(time.time() * 1000)


from solana.rpc.commitment import Confirmed  # noqa: E402


async def token_balance(sol: SolanaClient, ata: Pubkey) -> float:
    try:
        r = await sol._client.get_token_account_balance(ata, commitment=Confirmed)
        return float(r.value.ui_amount or 0.0)
    except Exception:
        return 0.0


async def snapshot(sol: SolanaClient, pool_ata: Pubkey, vouch_ata: Pubkey, stake_ata: Pubkey) -> dict:
    wallet = base_to_usd(await sol.get_wallet_usdc_balance())
    cl = await sol.read_credit_line()
    rep = await sol.reputation_score()
    vouch = base_to_usd(await sol.read_vouch_pool_total())
    return {
        "wallet_usd": round(wallet, 6),
        "pool_usd": round(await token_balance(sol, pool_ata), 6),
        "vouch_pool_usd": round(vouch, 6),
        "vouch_pool_ata_usd": round(await token_balance(sol, vouch_ata), 6),
        "stake_bond_usd": round(await token_balance(sol, stake_ata), 6),
        "reputation_score": round(rep, 6),
        "credit": None if cl is None else {
            "principal_usd": round(base_to_usd(cl.principal), 6),
            "accrued_interest_usd": round(base_to_usd(cl.accrued_interest), 9),
            "apr_bps": cl.apr_bps,
            "is_active": cl.is_active,
        },
    }


async def main() -> None:
    ch = compute_code_hash()
    pids = ProgramIds.from_env()

    sol = SolanaClient(RPC, OPERATOR_KP, MINT, ch, pids, 300)
    customer = SolanaClient(RPC, CUSTOMER_KP, MINT, ch, pids, 300)
    llm = LLMClient(
        api_key=os.environ["NVIDIA_API_KEY"],
        model=os.environ.get("LLM_MODEL", "meta/llama-3.1-8b-instruct"),
        base_url=os.environ.get("LLM_BASE_URL"),
    )

    pool_pda, _ = sol.pool_pda()
    vouch_pda, _ = sol.vouch_pool_pda()
    stake_pda, _ = sol.stake_bond_pda()
    pool_ata = derive_ata(pool_pda, MINT)
    vouch_ata = derive_ata(vouch_pda, MINT)
    stake_ata = derive_ata(stake_pda, MINT)

    results: dict = {
        "code_hash": ch.hex(),
        "cluster": "localnet",
        "llm_model": llm.model,
        "service_price_usd": base_to_usd(SERVICE_PRICE),
        "phases": [],
        "service_calls": [],
        "credit_events": [],
    }

    print("== T0: initial state ==")
    t0 = await snapshot(sol, pool_ata, vouch_ata, stake_ata)
    results["t0"] = t0
    print(json.dumps(t0, indent=2))

    # ---- Phase EARN: real x402 payments + real NVIDIA LLM summarization ----
    print(f"\n== EARN: {N_CALLS} serviced requests (x402 pay + LLM) ==")
    earn_start = now_ms()
    for i in range(N_CALLS):
        text = SAMPLE_TEXTS[i % len(SAMPLE_TEXTS)]
        pay_id = f"{i:032x}"
        # customer pays the agent on-chain (x402)
        sig = await customer.pay_for_service(ch, SERVICE_PRICE, pay_id)
        # agent performs the real work
        t = now_ms()
        summary, cost = await llm.summarize(text)
        latency_ms = now_ms() - t
        results["service_calls"].append({
            "i": i,
            "pay_sig": sig,
            "amount_usd": base_to_usd(SERVICE_PRICE),
            "llm_latency_ms": latency_ms,
            "llm_cost_est_usd": round(cost, 8),
            "summary_chars": len(summary),
            "summary_preview": summary[:160],
        })
        print(f"  call {i}: paid ${base_to_usd(SERVICE_PRICE):.2f} sig={sig[:12]}… "
              f"llm {latency_ms}ms, {len(summary)}c")
    earn = await snapshot(sol, pool_ata, vouch_ata, stake_ata)
    earn["wall_ms"] = now_ms() - earn_start
    earn["llm_total_in_tokens"] = llm.total_input_tokens
    earn["llm_total_out_tokens"] = llm.total_output_tokens
    results["phases"].append({"name": "earn", "after": earn})
    print(json.dumps(earn, indent=2))

    # ---- Phase CREDIT: two complete draw -> repay cycles ----
    for cyc in (1, 2):
        print(f"\n== CREDIT cycle {cyc}: borrow -> accrue -> repay ==")
        bsig = await sol.borrow_credit(BORROW_AMT)
        results["credit_events"].append({"cycle": cyc, "kind": "draw",
                                         "amount_usd": base_to_usd(BORROW_AMT), "sig": bsig})
        await asyncio.sleep(2)  # let the clock advance for a non-zero accrual
        asig = await sol.accrue_credit()
        cl = await sol.read_credit_line()
        debt = cl.principal + cl.accrued_interest
        rsig = await sol.repay_credit(debt)
        results["credit_events"].append({"cycle": cyc, "kind": "repay",
                                         "amount_usd": base_to_usd(debt), "sig": rsig})
        await asyncio.sleep(2)  # let the repay settle before snapshotting
        snap = await snapshot(sol, pool_ata, vouch_ata, stake_ata)
        print(f"  borrow sig={bsig[:12]}… accrue sig={asig[:12]}… repay ${base_to_usd(debt):.6f} sig={rsig[:12]}…")
        results["phases"].append({"name": f"credit_cycle_{cyc}", "after": snap})
        print(json.dumps(snap["credit"], indent=2))

    print("\n== TF: final state ==")
    await asyncio.sleep(2)  # settle before final read
    tf = await snapshot(sol, pool_ata, vouch_ata, stake_ata)
    results["tf"] = tf
    print(json.dumps(tf, indent=2))

    out = ROOT / "scripts" / "logs" / "walkthrough-results.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(results, indent=2) + "\n")
    print(f"\n→ wrote {out}")

    await sol.close()
    await customer.close()


if __name__ == "__main__":
    asyncio.run(main())
