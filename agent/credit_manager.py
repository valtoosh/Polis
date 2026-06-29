"""
credit_manager.py
=================

Auto-borrow / auto-repay logic.

Per the spec §"Reference agent / Loop logic":

    every 10s:
        balance = wallet.balance()
        rep    = reputation.score(agent_pubkey)
        if balance < $5 and credit.active_debt() < credit.limit(rep):
            credit.draw($10)
        if rep > 0 and time_since_last_repay > 24h and credit.active_debt() > 0:
            credit.repay(min(balance * 0.5, credit.active_debt()))

This module owns the policy logic. It does *not* own the polling loop —
main.py drives the loop and calls `evaluate(...)` once per tick.

Reputation→credit table is hard-coded per spec §"Reputation /
Credit-reputation coupling".
"""
from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from typing import Optional

from solana_client import (
    SolanaClient,
    base_to_usd,
    usd_to_base,
)

log = logging.getLogger(__name__)


# Spec table: (rep_min_exclusive, max_credit_usd, apr_bps)
_REPUTATION_TIERS: list[tuple[float, float, int]] = [
    (0.50, 500_000.0, 1_200),
    (0.20, 50_000.0, 1_800),
    (0.05, 5_000.0, 2_400),
    (0.01, 500.0, 3_650),
]


def credit_limit_for_reputation(rep: float) -> tuple[int, int]:
    """Return (max_credit_usdc_base_units, apr_bps) for a given reputation.

    `rep` is in [0, 1] per spec. If rep < 0.01, both are 0.
    """
    for lo, max_usd, apr in _REPUTATION_TIERS:
        if rep >= lo:
            return usd_to_base(max_usd), apr
    return 0, 0


@dataclass
class CreditSnapshot:
    """What we know about our CreditLine right now."""

    principal: int = 0
    accrued_interest: int = 0
    apr_bps: int = 0
    is_active: bool = False

    @property
    def total_debt(self) -> int:
        return self.principal + self.accrued_interest


class CreditManager:
    def __init__(
        self,
        client: SolanaClient,
        auto_borrow_amount_usd: float,
        daily_repay_fraction: float,
        accrue_interval_seconds: int = 600,
    ) -> None:
        self._client = client
        self._auto_borrow_base = usd_to_base(auto_borrow_amount_usd)
        self._daily_repay_fraction = daily_repay_fraction
        self._accrue_interval = accrue_interval_seconds

        self._snapshot = CreditSnapshot()
        self._last_repay_ts: float = 0.0
        self._last_accrue_ts: float = 0.0
        self._yesterday_earnings_base: int = 0

    # ----- accessors used by /status ---------------------------------------

    @property
    def snapshot(self) -> CreditSnapshot:
        return self._snapshot

    # ----- earnings accounting ---------------------------------------------

    def record_earning(self, amount_base: int) -> None:
        """Called by server.py whenever a /summarize call is paid.

        Used to size the daily repayment.
        """
        self._yesterday_earnings_base += amount_base

    def _rollover_daily_earnings(self) -> int:
        amt = self._yesterday_earnings_base
        self._yesterday_earnings_base = 0
        return amt

    # ----- on-chain refresh ------------------------------------------------

    async def refresh(self) -> None:
        line = await self._client.read_credit_line()
        if line is None:
            self._snapshot = CreditSnapshot()
            return
        self._snapshot = CreditSnapshot(
            principal=line.principal,
            accrued_interest=line.accrued_interest,
            apr_bps=line.apr_bps,
            is_active=line.is_active,
        )

    # ----- main policy entrypoint ------------------------------------------

    async def evaluate(self, balance_base: int) -> None:
        """One policy tick. Driven by main.py's 10-second loop."""
        await self.refresh()
        rep = await self._client.reputation_score()
        max_credit_base, _ = credit_limit_for_reputation(rep)

        # --- accrual ---
        now = time.time()
        if (
            self._snapshot.is_active
            and now - self._last_accrue_ts >= self._accrue_interval
        ):
            try:
                await self._accrue()
                self._last_accrue_ts = now
            except Exception as exc:
                log.warning("accrue failed: %s", exc)

        # --- borrow ---
        # The treasury manager's threshold drives the borrow trigger; here we
        # just check that we still have credit headroom available.
        headroom = max_credit_base - self._snapshot.total_debt
        if headroom > 0 and balance_base < self._auto_borrow_base:
            draw_amount = min(self._auto_borrow_base, headroom)
            try:
                await self._borrow(draw_amount)
            except Exception as exc:
                log.warning("auto-borrow failed: %s", exc)

        # --- repay ---
        if (
            rep > 0
            and self._snapshot.total_debt > 0
            and (now - self._last_repay_ts) >= 24 * 3600
        ):
            earnings = self._rollover_daily_earnings()
            target = int(earnings * self._daily_repay_fraction)
            amount = min(target, balance_base, self._snapshot.total_debt)
            if amount > 0:
                try:
                    await self._repay(amount)
                    self._last_repay_ts = now
                except Exception as exc:
                    log.warning("auto-repay failed: %s", exc)

    async def signal_low_balance(self, balance_base: int) -> None:
        """Treasury manager calls this when balance dips below threshold.

        We don't need to do anything special here — the next `evaluate()` will
        pick it up. The hook exists so the orchestrator can correlate logs
        and (in the future) trigger out-of-band actions.
        """
        log.info("credit_manager: low-balance signal received (bal=%d)", balance_base)

    # ----- internal: chain calls -------------------------------------------

    async def _borrow(self, amount: int) -> None:
        ix = self._client.ix_credit_borrow(amount)
        sig = await self._client.send_instruction(ix)
        log.info("borrowed $%.2f (sig=%s)", base_to_usd(amount), sig)
        await self.refresh()

    async def _repay(self, amount: int) -> None:
        ix = self._client.ix_credit_repay(amount)
        sig = await self._client.send_instruction(ix)
        log.info("repaid $%.2f (sig=%s)", base_to_usd(amount), sig)
        await self.refresh()

    async def _accrue(self) -> None:
        ix = self._client.ix_credit_accrue()
        sig = await self._client.send_instruction(ix)
        log.debug("accrued (sig=%s)", sig)
