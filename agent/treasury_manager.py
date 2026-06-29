"""
treasury_manager.py
===================

Tracks the agent's Wallet PDA balance. Exposes:

* `balance()` — current USDC balance, in base units (1 USDC == 1_000_000).
* `transfer_out(destination, amount)` — builds and submits the wallet
  program's `transfer_out` instruction (program enforces is_paused + per-trade
  cap; agent does not need to duplicate those checks).
* `on_low_balance(cb)` — registers an async callback fired whenever the
  balance drops below the configured threshold during periodic polling.

Periodic polling is driven externally — main.py owns the asyncio loop. This
class just exposes a `poll()` coroutine for the orchestrator to call.
"""
from __future__ import annotations

import logging
from typing import Awaitable, Callable, Optional

from solders.pubkey import Pubkey

from solana_client import SolanaClient, base_to_usd, usd_to_base

log = logging.getLogger(__name__)

LowBalanceCallback = Callable[[int], Awaitable[None]]


class TreasuryManager:
    def __init__(
        self,
        client: SolanaClient,
        low_balance_threshold_usd: float,
    ) -> None:
        self._client = client
        self._threshold = usd_to_base(low_balance_threshold_usd)
        self._cached_balance: int = 0
        self._cb: Optional[LowBalanceCallback] = None
        self._already_signalled = False  # debounce repeat firings

    def on_low_balance(self, cb: LowBalanceCallback) -> None:
        self._cb = cb

    @property
    def threshold(self) -> int:
        return self._threshold

    @property
    def cached_balance(self) -> int:
        """Last balance observed by `poll()`. Cheap to call from /status."""
        return self._cached_balance

    async def balance(self) -> int:
        bal = await self._client.get_wallet_usdc_balance()
        self._cached_balance = bal
        return bal

    async def poll(self) -> int:
        """Refresh balance; fire low-balance callback if crossed downward."""
        prev = self._cached_balance
        bal = await self.balance()
        log.debug("treasury balance=%s ($%.2f)", bal, base_to_usd(bal))

        if bal < self._threshold:
            if not self._already_signalled and self._cb is not None:
                self._already_signalled = True
                log.info(
                    "balance $%.2f below threshold $%.2f; signalling low balance",
                    base_to_usd(bal),
                    base_to_usd(self._threshold),
                )
                try:
                    await self._cb(bal)
                except Exception as exc:
                    log.exception("low-balance callback raised: %s", exc)
        else:
            # Hysteresis: only re-arm once balance exceeds 2x threshold.
            if self._already_signalled and bal >= 2 * self._threshold:
                self._already_signalled = False
        return bal

    async def transfer_out(self, destination: Pubkey, amount: int) -> str:
        """Build + submit a wallet.transfer_out instruction. Returns tx sig.

        The wallet program enforces the two-layer guard (is_paused +
        per_trade_cap_bps), so the agent does not need to gate locally beyond
        clamping `amount > 0`.
        """
        if amount <= 0:
            raise ValueError("transfer amount must be positive")
        ix = self._client.ix_transfer_out(destination, amount)
        return await self._client.send_instruction(ix)
