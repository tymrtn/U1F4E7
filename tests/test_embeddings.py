# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import pytest
from unittest.mock import patch, AsyncMock

from app.embeddings import (
    _vector_to_blob,
    _blob_to_vector,
    _cosine_similarity,
    _content_hash,
    embed_message,
    find_similar,
)


class TestVectorSerialization:
    def test_roundtrip(self):
        original = [0.1, 0.2, 0.3, -0.5, 1.0]
        blob = _vector_to_blob(original)
        restored = _blob_to_vector(blob)
        for a, b in zip(original, restored):
            assert abs(a - b) < 1e-6

    def test_empty_vector(self):
        blob = _vector_to_blob([])
        assert _blob_to_vector(blob) == []


class TestCosineSimilarity:
    def test_identical_vectors(self):
        v = [1.0, 2.0, 3.0]
        assert abs(_cosine_similarity(v, v) - 1.0) < 1e-6

    def test_orthogonal_vectors(self):
        a = [1.0, 0.0]
        b = [0.0, 1.0]
        assert abs(_cosine_similarity(a, b)) < 1e-6

    def test_opposite_vectors(self):
        a = [1.0, 0.0]
        b = [-1.0, 0.0]
        assert abs(_cosine_similarity(a, b) - (-1.0)) < 1e-6

    def test_zero_vector(self):
        a = [0.0, 0.0]
        b = [1.0, 1.0]
        assert _cosine_similarity(a, b) == 0.0


class TestContentHash:
    def test_deterministic(self):
        assert _content_hash("hello") == _content_hash("hello")

    def test_different_inputs(self):
        assert _content_hash("hello") != _content_hash("world")

    def test_length(self):
        assert len(_content_hash("test")) == 16


FAKE_VECTOR = [0.1] * 128


class TestEmbedMessage:
    @pytest.mark.asyncio
    async def test_embed_stores_in_db(self):
        with patch(
            "app.embeddings.embed_text",
            new_callable=AsyncMock,
            return_value=FAKE_VECTOR,
        ):
            result = await embed_message(
                account_id="acc-1",
                message_id="<msg@test.com>",
                subject="Test subject",
                body="Test body content",
            )
            assert result is True

            # Verify it's in the DB
            from app.db import get_db
            db = await get_db()
            cursor = await db.execute(
                "SELECT * FROM message_embeddings WHERE message_id = ?",
                ("<msg@test.com>",),
            )
            row = await cursor.fetchone()
            assert row is not None
            assert row["account_id"] == "acc-1"

    @pytest.mark.asyncio
    async def test_embed_skips_duplicate(self):
        with patch(
            "app.embeddings.embed_text",
            new_callable=AsyncMock,
            return_value=FAKE_VECTOR,
        ) as mock_embed:
            await embed_message("acc-1", "<dup@test.com>", "Subject", "Body")
            result = await embed_message("acc-1", "<dup@test.com>", "Subject", "Body")
            assert result is False
            # embed_text called only once for initial embed (second call skips)
            assert mock_embed.call_count == 1

    @pytest.mark.asyncio
    async def test_embed_updates_on_content_change(self):
        with patch(
            "app.embeddings.embed_text",
            new_callable=AsyncMock,
            return_value=FAKE_VECTOR,
        ) as mock_embed:
            await embed_message("acc-1", "<change@test.com>", "Subject", "Body v1")
            result = await embed_message("acc-1", "<change@test.com>", "Subject", "Body v2")
            assert result is True
            assert mock_embed.call_count == 2


class TestFindSimilar:
    @pytest.mark.asyncio
    async def test_find_returns_scored_results(self):
        # Seed some embeddings
        with patch(
            "app.embeddings.embed_text",
            new_callable=AsyncMock,
            return_value=FAKE_VECTOR,
        ):
            await embed_message("acc-2", "<a@test.com>", "Villa pricing", "Price info")
            await embed_message("acc-2", "<b@test.com>", "Beach access", "Beach details")

        # Search — mock embed_text to return same vector (will have high similarity)
        with patch(
            "app.embeddings.embed_text",
            new_callable=AsyncMock,
            return_value=FAKE_VECTOR,
        ):
            results = await find_similar("acc-2", "villa price", limit=5)
            assert len(results) == 2
            assert all(r["score"] > 0 for r in results)

    @pytest.mark.asyncio
    async def test_find_empty_account(self):
        with patch(
            "app.embeddings.embed_text",
            new_callable=AsyncMock,
            return_value=FAKE_VECTOR,
        ):
            results = await find_similar("acc-nonexistent", "test", limit=5)
            assert results == []

    @pytest.mark.asyncio
    async def test_find_respects_limit(self):
        with patch(
            "app.embeddings.embed_text",
            new_callable=AsyncMock,
            return_value=FAKE_VECTOR,
        ):
            for i in range(5):
                await embed_message("acc-3", f"<msg-{i}@test.com>", f"Subject {i}", f"Body {i}")

        with patch(
            "app.embeddings.embed_text",
            new_callable=AsyncMock,
            return_value=FAKE_VECTOR,
        ):
            results = await find_similar("acc-3", "test", limit=2)
            assert len(results) == 2

    @pytest.mark.asyncio
    async def test_find_isolates_accounts(self):
        with patch(
            "app.embeddings.embed_text",
            new_callable=AsyncMock,
            return_value=FAKE_VECTOR,
        ):
            await embed_message("acc-A", "<a@test.com>", "Subject A", "Body A")
            await embed_message("acc-B", "<b@test.com>", "Subject B", "Body B")

        with patch(
            "app.embeddings.embed_text",
            new_callable=AsyncMock,
            return_value=FAKE_VECTOR,
        ):
            results_a = await find_similar("acc-A", "test", limit=10)
            results_b = await find_similar("acc-B", "test", limit=10)
            assert len(results_a) == 1
            assert results_a[0]["message_id"] == "<a@test.com>"
            assert len(results_b) == 1
            assert results_b[0]["message_id"] == "<b@test.com>"
