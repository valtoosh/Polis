"""
llm_client.py
=============

LLM wrapper used by the reference agent to summarize text.

Provider
--------
Uses an OpenAI-compatible chat-completions endpoint. The default target is
NVIDIA's hosted API catalog (https://integrate.api.nvidia.com/v1), which
serves open-weight models (e.g. Llama 3.x) under a free tier with rate
limits. Because the endpoint is OpenAI-compatible, any other compatible
provider (OpenAI, Together, local vLLM, etc.) works by overriding
`base_url` / `model`.

Cost model
----------
The free tier bills nothing, but the agent's economics depend on settling a
*compute cost* per call so the wallet depletes and the credit cycle
(draw -> repay) is exercised. We therefore keep a nominal per-million-token
estimate; it is an accounting figure used to size outbound transfers, not a
real invoice. Override via `pricing_per_mtok` if you want true-cost numbers.
"""
from __future__ import annotations

import asyncio
import logging
import os
from dataclasses import dataclass
from typing import Optional

from openai import AsyncOpenAI
from openai import APIError, RateLimitError, APIConnectionError

log = logging.getLogger(__name__)

# Default OpenAI-compatible endpoint: NVIDIA's hosted API catalog.
DEFAULT_BASE_URL = "https://integrate.api.nvidia.com/v1"

# Default open model on the NVIDIA free tier: fast, cheap, reliable.
DEFAULT_MODEL = "meta/llama-3.1-8b-instruct"

# Nominal USD per 1M tokens, used only to size the agent's compute-cost
# transfers. Open-model hosted inference is roughly an order of magnitude
# cheaper than frontier models; these figures keep the economic loop honest
# without claiming a real bill (the free tier charges nothing).
DEFAULT_PRICING_USD_PER_MTOK = {
    "meta/llama-3.1-8b-instruct": {"input": 0.05, "output": 0.10},
}
# Fallback nominal rate for any model not in the table above.
FALLBACK_PRICING_USD_PER_MTOK = {"input": 0.05, "output": 0.10}

# Retry policy for the free tier's rate limits / transient errors.
MAX_RETRIES = 5
BASE_BACKOFF_SECONDS = 1.0


@dataclass
class LLMUsage:
    input_tokens: int
    output_tokens: int
    cost_usd: float


class LLMClient:
    """Thin async wrapper. Tracks aggregate usage for /status reporting."""

    def __init__(
        self,
        api_key: str,
        model: str,
        base_url: Optional[str] = None,
        pricing_per_mtok: Optional[dict[str, dict[str, float]]] = None,
    ) -> None:
        self._client = AsyncOpenAI(
            api_key=api_key,
            base_url=base_url or os.environ.get("LLM_BASE_URL", DEFAULT_BASE_URL),
        )
        self._model = model
        self._pricing = pricing_per_mtok or DEFAULT_PRICING_USD_PER_MTOK
        self._total_input_tokens = 0
        self._total_output_tokens = 0
        self._total_cost_usd = 0.0

    @property
    def model(self) -> str:
        return self._model

    @property
    def total_cost_usd(self) -> float:
        return self._total_cost_usd

    @property
    def total_input_tokens(self) -> int:
        return self._total_input_tokens

    @property
    def total_output_tokens(self) -> int:
        return self._total_output_tokens

    def _price_call(self, input_tokens: int, output_tokens: int) -> float:
        prices = self._pricing.get(self._model, FALLBACK_PRICING_USD_PER_MTOK)
        cost = (
            input_tokens * prices["input"] / 1_000_000.0
            + output_tokens * prices["output"] / 1_000_000.0
        )
        return cost

    async def summarize(self, text: str) -> tuple[str, float]:
        """Summarize `text` with the configured model.

        Returns (summary, cost_usd_estimate). Cost is appended to the running
        total exposed via the properties above. Retries with exponential
        backoff on rate-limit / transient connection errors (free tiers throttle).
        """
        # Cap user input length defensively. 200k chars is generous; protects
        # against runaway costs from a misbehaving customer.
        if len(text) > 200_000:
            raise ValueError("input too long")

        system_prompt = (
            "You are a concise summarizer. Produce a 2-3 sentence neutral "
            "summary of the provided text. Do not add commentary, opinions, "
            "or follow-up questions."
        )

        last_err: Optional[Exception] = None
        for attempt in range(MAX_RETRIES):
            try:
                completion = await self._client.chat.completions.create(
                    model=self._model,
                    max_tokens=512,
                    temperature=0.2,
                    messages=[
                        {"role": "system", "content": system_prompt},
                        {"role": "user", "content": text},
                    ],
                )
                break
            except (RateLimitError, APIConnectionError) as e:
                last_err = e
                backoff = BASE_BACKOFF_SECONDS * (2 ** attempt)
                log.warning(
                    "summarize transient error (attempt %d/%d): %s; backing off %.1fs",
                    attempt + 1,
                    MAX_RETRIES,
                    e,
                    backoff,
                )
                await asyncio.sleep(backoff)
            except APIError as e:
                # Non-retryable API error (bad request, auth, etc.).
                log.error("summarize API error: %s", e)
                raise
        else:
            raise RuntimeError(
                f"summarize failed after {MAX_RETRIES} retries: {last_err}"
            )

        summary = (completion.choices[0].message.content or "").strip()

        usage = completion.usage
        input_tokens = getattr(usage, "prompt_tokens", 0) or 0
        output_tokens = getattr(usage, "completion_tokens", 0) or 0
        cost = self._price_call(input_tokens, output_tokens)

        self._total_input_tokens += input_tokens
        self._total_output_tokens += output_tokens
        self._total_cost_usd += cost

        log.info(
            "summarize ok: in=%d out=%d cost=$%.6f", input_tokens, output_tokens, cost
        )
        return summary, cost
