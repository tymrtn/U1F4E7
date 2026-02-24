# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import os
import logging
from dataclasses import dataclass
from typing import Optional

import httpx

logger = logging.getLogger(__name__)

OPENROUTER_BASE = "https://openrouter.ai/api/v1/chat/completions"


@dataclass
class LLMResponse:
    content: str
    model: str
    usage: dict


async def chat_completion(
    system_prompt: str,
    user_message: str,
    model: Optional[str] = None,
    max_tokens: int = 2048,
    temperature: float = 0.3,
) -> LLMResponse:
    api_key = os.getenv("OPENROUTER_API_KEY")
    if not api_key:
        raise RuntimeError("OPENROUTER_API_KEY environment variable is required")

    model = model or os.getenv("OPENROUTER_MODEL", "anthropic/claude-sonnet-4-20250514")

    async with httpx.AsyncClient(timeout=60) as client:
        resp = await client.post(
            OPENROUTER_BASE,
            headers={
                "Authorization": f"Bearer {api_key}",
                "Content-Type": "application/json",
            },
            json={
                "model": model,
                "messages": [
                    {"role": "system", "content": system_prompt},
                    {"role": "user", "content": user_message},
                ],
                "max_tokens": max_tokens,
                "temperature": temperature,
            },
        )
        resp.raise_for_status()
        data = resp.json()

    choice = data["choices"][0]
    return LLMResponse(
        content=choice["message"]["content"],
        model=data.get("model", model),
        usage=data.get("usage", {}),
    )
