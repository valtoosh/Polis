"""
main.py
=======

Top-level orchestrator. Wires all managers, runs:

* uvicorn HTTP server for FastAPI (serves /summarize etc.).
* A 10-second polling loop that drives the treasury + credit managers.
* A 24-hour cron-ish loop that triggers daily repayment evaluation.
* Background payment-event subscriber (websocket).

Graceful shutdown on SIGTERM / Ctrl-C.

Bootstrap sequence
------------------
1. Load env (.env).
2. Compute the agent's own code-hash from agent/*.py.
3. Verify the AgentProfile PDA exists on-chain and its operator key matches
   the loaded keypair. If not, exit with an actionable error pointing at the
   registration script.
4. Start treasury polling, credit evaluation, payment subscriber, HTTP server.
5. Run until SIGTERM.
"""
from __future__ import annotations

import asyncio
import logging
import os
import signal
import sys
import time
from pathlib import Path

import uvicorn
from dotenv import load_dotenv
from solders.pubkey import Pubkey

from code_hash import compute_code_hash, code_hash_hex
from credit_manager import CreditManager
from llm_client import LLMClient
from server import AgentState, build_app
from solana_client import ProgramIds, SolanaClient
from treasury_manager import TreasuryManager

log = logging.getLogger("polis.agent")

POLL_INTERVAL_SECONDS = 10
REPAY_CRON_INTERVAL_SECONDS = 3600  # check repayment eligibility hourly


def _setup_logging() -> None:
    level = os.environ.get("LOG_LEVEL", "INFO").upper()
    logging.basicConfig(
        level=level,
        format="%(asctime)s %(levelname)-7s %(name)s | %(message)s",
    )


def _env_bool(name: str, default: bool) -> bool:
    raw = os.environ.get(name)
    if raw is None:
        return default
    return raw.lower() in {"1", "true", "yes", "on"}


def _env_float(name: str, default: float) -> float:
    raw = os.environ.get(name)
    return float(raw) if raw else default


def _env_int(name: str, default: int) -> int:
    raw = os.environ.get(name)
    return int(raw) if raw else default


async def _verify_registration(client: SolanaClient, dev_mode: bool) -> None:
    agent_pda, _ = client.agent_pda()
    info = await client._client.get_account_info(agent_pda)
    if info.value is None:
        msg = (
            f"AgentProfile PDA {agent_pda} not found on-chain. "
            "Register the agent first via scripts/register-agent.ts "
            "(see README)."
        )
        if dev_mode:
            log.warning("DEV_MODE: %s; continuing anyway", msg)
            return
        raise RuntimeError(msg)
    log.info("AgentProfile PDA %s present on-chain", agent_pda)


async def _run() -> None:
    _setup_logging()
    load_dotenv()

    # --- env ---
    rpc_url = os.environ["SOLANA_RPC_URL"]
    operator_path = os.environ["OPERATOR_KEYPAIR_PATH"]
    usdc_mint = Pubkey.from_string(os.environ["SOLANA_USDC_MINT"])
    service_price_usd = _env_float("SERVICE_PRICE_USD", 0.10)
    low_balance_usd = _env_float("LOW_BALANCE_THRESHOLD_USD", 5.0)
    auto_borrow_usd = _env_float("AUTO_BORROW_AMOUNT_USD", 10.0)
    repay_fraction = _env_float("DAILY_REPAY_FRACTION", 0.5)
    payment_validity = _env_int("PAYMENT_VALIDITY_WINDOW_SECONDS", 300)
    dev_mode = _env_bool("DEV_MODE", False)
    http_host = os.environ.get("HTTP_HOST", "0.0.0.0")
    http_port = int(os.environ.get("HTTP_PORT", "8000"))
    llm_model = os.environ.get("LLM_MODEL", "meta/llama-3.1-8b-instruct")

    program_ids = ProgramIds.from_env()

    if dev_mode:
        log.warning(
            "DEV_MODE is ENABLED. /summarize will NOT verify payments. "
            "Never run with DEV_MODE in production."
        )

    # --- code-hash ---
    ch = compute_code_hash()
    ch_hex = ch.hex()
    log.info("agent code-hash: %s", ch_hex)

    # --- chain client ---
    sol = SolanaClient(
        rpc_url=rpc_url,
        operator_keypair_path=operator_path,
        usdc_mint=usdc_mint,
        code_hash=ch,
        program_ids=program_ids,
        payment_validity_seconds=payment_validity,
    )

    # --- registration check ---
    await _verify_registration(sol, dev_mode=dev_mode)

    # --- managers ---
    treasury = TreasuryManager(sol, low_balance_threshold_usd=low_balance_usd)
    credit = CreditManager(
        sol,
        auto_borrow_amount_usd=auto_borrow_usd,
        daily_repay_fraction=repay_fraction,
    )
    treasury.on_low_balance(credit.signal_low_balance)

    # LLM key: NVIDIA free-tier key (nvapi-...) by default; LLM_API_KEY is an
    # alias for any OpenAI-compatible provider. base_url defaults to NVIDIA.
    llm_api_key = (
        os.environ.get("NVIDIA_API_KEY")
        or os.environ.get("LLM_API_KEY")
        or os.environ.get("OPENAI_API_KEY")
    )
    if not llm_api_key:
        raise RuntimeError(
            "No LLM API key found. Set NVIDIA_API_KEY (free at build.nvidia.com), "
            "or LLM_API_KEY / OPENAI_API_KEY for another OpenAI-compatible provider."
        )
    llm = LLMClient(
        api_key=llm_api_key,
        model=llm_model,
        base_url=os.environ.get("LLM_BASE_URL"),
    )

    state = AgentState(
        solana=sol,
        treasury=treasury,
        credit=credit,
        llm=llm,
        code_hash_hex=ch_hex,
        service_price_usd=service_price_usd,
        dev_mode=dev_mode,
        started_at=time.time(),
    )

    app = build_app(state)

    # --- background tasks ---
    await sol.start_payment_subscriber()
    stop_event = asyncio.Event()

    async def poll_loop() -> None:
        while not stop_event.is_set():
            try:
                bal = await treasury.poll()
                await credit.evaluate(bal)
            except Exception:
                log.exception("poll_loop tick failed")
            try:
                await asyncio.wait_for(stop_event.wait(), POLL_INTERVAL_SECONDS)
            except asyncio.TimeoutError:
                continue

    poll_task = asyncio.create_task(poll_loop(), name="poll_loop")

    # --- HTTP server ---
    config = uvicorn.Config(
        app,
        host=http_host,
        port=http_port,
        log_level=os.environ.get("LOG_LEVEL", "info").lower(),
        loop="asyncio",
    )
    server = uvicorn.Server(config)

    # graceful shutdown
    loop = asyncio.get_running_loop()

    def _request_stop(*_: object) -> None:
        log.info("shutdown requested")
        stop_event.set()
        server.should_exit = True

    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            loop.add_signal_handler(sig, _request_stop)
        except NotImplementedError:
            # Windows: signal handlers are limited.
            pass

    log.info(
        "starting HTTP server on %s:%d (price=$%.2f, dev=%s)",
        http_host,
        http_port,
        service_price_usd,
        dev_mode,
    )

    try:
        await server.serve()
    finally:
        stop_event.set()
        poll_task.cancel()
        try:
            await poll_task
        except (asyncio.CancelledError, Exception):
            pass
        await sol.close()
        log.info("shutdown complete")


def main() -> None:
    try:
        asyncio.run(_run())
    except KeyboardInterrupt:
        pass
    except Exception:
        log.exception("agent crashed")
        sys.exit(1)


if __name__ == "__main__":
    main()
