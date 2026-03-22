# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import hashlib
import logging
import math
import os
import struct
from typing import Optional

import httpx

from app.db import get_db

logger = logging.getLogger(__name__)

OPENROUTER_EMBEDDINGS_URL = "https://openrouter.ai/api/v1/embeddings"
DEFAULT_EMBEDDING_MODEL = "openai/text-embedding-3-small"


async def embed_text(text: str, model: Optional[str] = None) -> list[float]:
    api_key = os.getenv("OPENROUTER_API_KEY")
    if not api_key:
        raise RuntimeError("OPENROUTER_API_KEY environment variable is required")

    model = model or os.getenv("EMBEDDING_MODEL", DEFAULT_EMBEDDING_MODEL)

    async with httpx.AsyncClient(timeout=30) as client:
        resp = await client.post(
            OPENROUTER_EMBEDDINGS_URL,
            headers={
                "Authorization": f"Bearer {api_key}",
                "Content-Type": "application/json",
            },
            json={
                "model": model,
                "input": text[:8000],
            },
        )
        resp.raise_for_status()
        data = resp.json()

    return data["data"][0]["embedding"]


def _vector_to_blob(vector: list[float]) -> bytes:
    return struct.pack(f"{len(vector)}f", *vector)


def _blob_to_vector(blob: bytes) -> list[float]:
    count = len(blob) // 4
    return list(struct.unpack(f"{count}f", blob))


def _cosine_similarity(a: list[float], b: list[float]) -> float:
    dot = sum(x * y for x, y in zip(a, b))
    norm_a = math.sqrt(sum(x * x for x in a))
    norm_b = math.sqrt(sum(x * x for x in b))
    if norm_a == 0 or norm_b == 0:
        return 0.0
    return dot / (norm_a * norm_b)


def _content_hash(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:16]


async def embed_message(
    account_id: str,
    message_id: str,
    subject: str,
    body: str,
    from_addr: str = "",
    date: str = "",
) -> bool:
    text = f"{subject}\n{body[:2000]}"
    content_hash = _content_hash(text)

    db = await get_db()
    cursor = await db.execute(
        "SELECT content_hash FROM message_embeddings WHERE message_id = ?",
        (message_id,),
    )
    existing = await cursor.fetchone()
    if existing and existing["content_hash"] == content_hash:
        return False  # already embedded with same content

    vector = await embed_text(text)
    blob = _vector_to_blob(vector)
    model = os.getenv("EMBEDDING_MODEL", DEFAULT_EMBEDDING_MODEL)

    from datetime import datetime, timezone
    now = datetime.now(timezone.utc).isoformat()

    await db.execute(
        """INSERT OR REPLACE INTO message_embeddings
        (message_id, account_id, content_hash, embedding, model, embedded_at)
        VALUES (?, ?, ?, ?, ?, ?)""",
        (message_id, account_id, content_hash, blob, model, now),
    )
    await db.commit()
    return True


async def find_similar(
    account_id: str,
    query: str,
    limit: int = 5,
) -> list[dict]:
    db = await get_db()
    cursor = await db.execute(
        "SELECT message_id, embedding FROM message_embeddings WHERE account_id = ?",
        (account_id,),
    )
    rows = await cursor.fetchall()
    if not rows:
        return []

    query_vector = await embed_text(query)
    scored = []
    for row in rows:
        stored_vector = _blob_to_vector(row["embedding"])
        score = _cosine_similarity(query_vector, stored_vector)
        scored.append((row["message_id"], score))

    scored.sort(key=lambda x: x[1], reverse=True)
    top = scored[:limit]

    # Fetch message metadata from thread_links or return IDs with scores
    # For now, return what we have — caller can fetch full messages
    results = []
    for msg_id, score in top:
        if score < 0.1:
            continue
        results.append({
            "message_id": msg_id,
            "score": round(score, 4),
            "subject": "",
            "from_addr": "",
            "date": "",
            "preview": "",
        })

    return results


async def backfill_embeddings(
    account_id: str,
    messages_to_embed: list[dict],
) -> dict:
    embedded = 0
    skipped = 0
    errors = 0

    for msg in messages_to_embed:
        try:
            was_new = await embed_message(
                account_id=account_id,
                message_id=msg.get("message_id", msg.get("uid", "")),
                subject=msg.get("subject", ""),
                body=msg.get("text_body", ""),
                from_addr=msg.get("from_addr", ""),
                date=msg.get("date", ""),
            )
            if was_new:
                embedded += 1
            else:
                skipped += 1
        except Exception:
            logger.exception("Failed to embed message %s", msg.get("message_id"))
            errors += 1

    return {"embedded": embedded, "skipped": skipped, "errors": errors}
