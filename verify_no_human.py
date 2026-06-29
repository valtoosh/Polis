"""
verify_no_human.py — direct P-No-Human verification for the walkthrough.

Fetches every transaction recorded in walkthrough-results.json, enumerates the
signers, and classifies each: agent-operator (programmatic agent operations) or
customer (x402 service payment). Any other signer is a violation. This is the
localnet-robust replacement for audit.ts's blockTime-windowed scan, which
returns empty on solana-test-validator.
"""
from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT / "agent"))

from solana.rpc.async_api import AsyncClient  # noqa: E402
from solana.rpc.commitment import Confirmed  # noqa: E402
from solders.signature import Signature  # noqa: E402

RPC = "http://127.0.0.1:8899"
OPERATOR = "8gwVxoZPYq6db6nXxMBJVMTuYTEbL2xSUwKTUT9Y3V3C"
CUSTOMER1 = "2SWMSAsSaYuwzHr2WvfQY3QgeU8RnXY7yCvS9cYUy2AS"

LABELS = {OPERATOR: "agent-operator", CUSTOMER1: "customer"}


async def signers_of(client: AsyncClient, sig_str: str) -> list[str]:
    sig = Signature.from_string(sig_str)
    resp = await client.get_transaction(sig, commitment=Confirmed, max_supported_transaction_version=0)
    tx = resp.value.transaction.transaction
    msg = tx.message
    n = msg.header.num_required_signatures
    return [str(k) for k in msg.account_keys[:n]]


async def main() -> None:
    data = json.loads((ROOT / "scripts" / "logs" / "walkthrough-results.json").read_text())
    client = AsyncClient(RPC)

    txs: list[tuple[str, str, str]] = []  # (kind, expected_role, sig)
    for c in data["service_calls"]:
        txs.append((f"x402 pay #{c['i']}", "customer", c["pay_sig"]))
    for e in data["credit_events"]:
        txs.append((f"credit {e['kind']} cyc{e['cycle']}", "agent-operator", e["sig"]))

    print(f"Verifying {len(txs)} operational transactions...\n")
    violations = 0
    classified = {"agent-operator": 0, "customer": 0}
    for kind, expected, sig in txs:
        signers = await signers_of(client, sig)
        roles = [LABELS.get(s, f"UNKNOWN({s[:8]})") for s in signers]
        ok = all(r in ("agent-operator", "customer") for r in roles)
        for r in roles:
            if r in classified:
                classified[r] += 1
        if not ok:
            violations += 1
        print(f"  [{'OK' if ok else 'VIOLATION'}] {kind:22s} signer(s)={roles}")

    print(f"\nClassified: {classified['agent-operator']} agent-operator, "
          f"{classified['customer']} customer signatures.")
    print(f"Violations (human approval outside customer payment): {violations}")
    print("RESULT:", "PASS — no human signed any operational transaction." if violations == 0
          else "FAIL")
    await client.close()


if __name__ == "__main__":
    asyncio.run(main())
