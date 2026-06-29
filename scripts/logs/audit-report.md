# polis 14-day Demo — Audit Report

**Cluster:** localnet  
**Demo start:** 2026-06-15T19:19:45.305Z  
**Generated:** 2026-06-15T19:31:40.570Z  
**Agent code-hash:** `d576bf0bb2a22b0231f61afa52d2fc6d53f4b0926284ff2e711a14c30bfa15e8`  
**Overall:** **FAIL**

## Findings

| ID | Status | Description |
|---|---|---|
| C1: 14-day continuity | FAIL | A snapshot file exists for each of the 14 demo days. |
| C2: ≥ 2 credit cycles | FAIL | At least two complete draw→repay credit cycles. |
| C3: Reputation non-decreasing | PASS | Total vouched amount (proxy for reputation score) is monotonically non-decreasing. |
| C4: Wallet ≥ starting balance | FAIL | Final wallet balance is at least the starting wallet balance. |
| C5: No-human-signing | PASS | Only the agent operator (signing allowlisted instructions) and LP/customer keypairs signed transactions touching polis PDAs. |

### C1: 14-day continuity — FAIL
*A snapshot file exists for each of the 14 demo days.*

```
Missing 13 day(s): 2026-06-16, 2026-06-17, 2026-06-18, 2026-06-19, 2026-06-20, 2026-06-21, 2026-06-22, 2026-06-23, 2026-06-24, 2026-06-25, 2026-06-26, 2026-06-27, 2026-06-28
```

### C2: ≥ 2 credit cycles — FAIL
*At least two complete draw→repay credit cycles.*

```
Detected 0 cycle(s) (from snapshot deltas (heuristic)).
```

### C3: Reputation non-decreasing — PASS
*Total vouched amount (proxy for reputation score) is monotonically non-decreasing.*

```
No violation across 1 snapshots.
```

### C4: Wallet ≥ starting balance — FAIL
*Final wallet balance is at least the starting wallet balance.*

```
Insufficient snapshots to compare (need ≥ 2).
```

### C5: No-human-signing — PASS
*Only the agent operator (signing allowlisted instructions) and LP/customer keypairs signed transactions touching polis PDAs.*

```
Scanned 0 txs; 0 operator signatures, 0 violation(s).
```
