"""
server.py
=========

FastAPI app. Endpoints:

* POST /summarize   — paid endpoint. Verifies a PaymentReceived event then
                      returns a summary.
* GET  /pricing     — current price per call (USD).
* GET  /status      — wallet balance, active credit, reputation, recent earnings.
* GET  /code-hash   — registered code-hash (hex).
* GET  /health      — liveness.

This module exposes a `build_app(state)` factory so main.py can wire its
managers in once at startup. Keeping the state on `app.state` keeps the
endpoint handlers easy to test.
"""
from __future__ import annotations

import logging
import time
from dataclasses import dataclass
from typing import Optional

from fastapi import FastAPI, HTTPException, Request, Response
from pydantic import BaseModel, Field

from credit_manager import CreditManager
from llm_client import LLMClient
from solana_client import SolanaClient, base_to_usd, usd_to_base
from treasury_manager import TreasuryManager

log = logging.getLogger(__name__)


# ---------- shared state ----------------------------------------------------


@dataclass
class AgentState:
    solana: SolanaClient
    treasury: TreasuryManager
    credit: CreditManager
    llm: LLMClient
    code_hash_hex: str
    service_price_usd: float
    dev_mode: bool
    started_at: float

    # Lightweight running counters for /status.
    total_calls: int = 0
    total_earned_base: int = 0  # USDC base units
    total_failed_payment: int = 0


# ---------- pydantic schemas ------------------------------------------------


class SummarizeRequest(BaseModel):
    text: str = Field(..., min_length=1, max_length=200_000)
    payment_id: str = Field(..., min_length=1, max_length=128)


class SummarizeResponse(BaseModel):
    summary: str
    cost_estimate_usd: float


# ---------- factory ---------------------------------------------------------


def build_app(state: AgentState) -> FastAPI:
    app = FastAPI(title="polis reference agent", version="0.1.0")
    app.state.agent = state

    @app.get("/health")
    async def health() -> dict[str, object]:
        return {
            "ok": True,
            "uptime_seconds": time.time() - state.started_at,
        }

    @app.get("/code-hash")
    async def code_hash() -> dict[str, str]:
        return {"code_hash": state.code_hash_hex}

    @app.get("/pricing")
    async def pricing() -> dict[str, object]:
        agent_pda, _ = state.solana.agent_pda()
        wallet_pda, _ = state.solana.wallet_pda()
        return {
            "price_usd": state.service_price_usd,
            "currency": "USDC",
            "wallet_pda": str(wallet_pda),
            "agent_pda": str(agent_pda),
            "mint": str(state.solana.usdc_mint),
            "x402_program_id": str(state.solana.programs.x402),
        }

    @app.get("/status")
    async def status() -> dict[str, object]:
        snap = state.credit.snapshot
        rep = await state.solana.reputation_score()
        return {
            "code_hash": state.code_hash_hex,
            "operator": str(state.solana.operator.pubkey()),
            "wallet_balance_usd": base_to_usd(state.treasury.cached_balance),
            "credit": {
                "principal_usd": base_to_usd(snap.principal),
                "accrued_interest_usd": base_to_usd(snap.accrued_interest),
                "total_debt_usd": base_to_usd(snap.total_debt),
                "apr_bps": snap.apr_bps,
                "is_active": snap.is_active,
            },
            "reputation": rep,
            "earnings": {
                "total_calls": state.total_calls,
                "total_earned_usd": base_to_usd(state.total_earned_base),
                "llm_total_cost_usd": state.llm.total_cost_usd,
                "failed_payment_attempts": state.total_failed_payment,
            },
            "uptime_seconds": time.time() - state.started_at,
            "dev_mode": state.dev_mode,
        }

    @app.post("/summarize", response_model=SummarizeResponse)
    async def summarize(req: SummarizeRequest, response: Response) -> SummarizeResponse:
        # --- payment verification ----------------------------------------
        if state.dev_mode:
            log.warning(
                "DEV_MODE: skipping payment verification for payment_id=%s",
                req.payment_id,
            )
        else:
            ok = await state.solana.has_recent_payment(req.payment_id)
            if not ok:
                state.total_failed_payment += 1
                _emit_payment_required(response, state)
                raise HTTPException(
                    status_code=402,
                    detail={
                        "error": "payment_required",
                        "payment_id": req.payment_id,
                        "price_usd": state.service_price_usd,
                        "wallet_pda": str(state.solana.wallet_pda()[0]),
                        "mint": str(state.solana.usdc_mint),
                    },
                )

            # Atomically consume — same payment_id cannot fund two calls.
            rec = await state.solana.consume_payment(req.payment_id)
            if rec is None:
                state.total_failed_payment += 1
                raise HTTPException(status_code=402, detail="payment_consumed")

            required = usd_to_base(state.service_price_usd)
            if rec.amount < required:
                state.total_failed_payment += 1
                raise HTTPException(
                    status_code=402,
                    detail={
                        "error": "underpaid",
                        "received": base_to_usd(rec.amount),
                        "required": state.service_price_usd,
                    },
                )
            state.total_earned_base += rec.amount
            state.credit.record_earning(rec.amount)

        # --- do the work --------------------------------------------------
        try:
            summary, cost_usd = await state.llm.summarize(req.text)
        except Exception as exc:
            log.exception("LLM call failed")
            raise HTTPException(status_code=502, detail=f"llm_error: {exc}") from exc

        state.total_calls += 1
        return SummarizeResponse(summary=summary, cost_estimate_usd=cost_usd)

    return app


def _emit_payment_required(response: Response, state: AgentState) -> None:
    """Attach an x402-style Payment-Required JSON header so clients without
    payment can retry.

    Spec §"Earning rails (x402)" notes the production wire protocol uses a
    `Payment-Required` header and `X-Payment` retry; the prototype skips the
    HTTP ceremony at the program level but the agent still advertises it.
    """
    import json

    wallet_pda, _ = state.solana.wallet_pda()
    payload = {
        "version": 1,
        "accepts": [
            {
                "chain": "solana",
                "program": str(state.solana.programs.x402),
                "instruction": "pay_for_service",
                "recipient": str(wallet_pda),
                "mint": str(state.solana.usdc_mint),
                "amount_usd": state.service_price_usd,
            }
        ],
    }
    response.headers["Payment-Required"] = json.dumps(payload, separators=(",", ":"))
