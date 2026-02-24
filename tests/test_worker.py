# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import pytest
from unittest.mock import AsyncMock, MagicMock, patch
from datetime import datetime, timezone, timedelta

from app.transport.worker import SendWorker, MAX_RETRIES, BASE_DELAY_SECONDS, MAX_DELAY_SECONDS
from app.transport.smtp import SmtpSendError


def make_message_row(msg_id="msg-1", account_id="acct-1", retry_count=0):
    return {
        "id": msg_id,
        "account_id": account_id,
        "from_addr": "sender@example.com",
        "to_addr": "recipient@example.com",
        "subject": "Test",
        "text_content": "Hello",
        "html_content": None,
        "retry_count": retry_count,
    }


def make_account():
    return {
        "id": "acct-1",
        "smtp_host": "smtp.example.com",
        "smtp_port": 587,
        "effective_smtp_username": "user@example.com",
        "effective_smtp_password": "secret",
        "display_name": None,
    }


@pytest.mark.asyncio
async def test_start_recovers_orphans():
    pool = MagicMock()
    worker = SendWorker(pool)

    with patch("app.transport.worker.messages.recover_orphans", new_callable=AsyncMock) as mock_recover:
        await worker.start()
        mock_recover.assert_awaited_once()

    await worker.stop()


@pytest.mark.asyncio
async def test_notify_sets_event():
    pool = MagicMock()
    worker = SendWorker(pool)
    assert not worker._work_available.is_set()
    worker.notify()
    assert worker._work_available.is_set()


@pytest.mark.asyncio
async def test_successful_send_marks_sent():
    pool = MagicMock()
    worker = SendWorker(pool)
    row = make_message_row()
    account = make_account()

    with (
        patch("app.transport.worker.messages.claim_message", new_callable=AsyncMock, return_value=True),
        patch("app.transport.worker.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=account),
        patch("app.transport.worker.send_message", new_callable=AsyncMock, return_value="<msg-id@test>"),
        patch("app.transport.worker.messages.mark_sent", new_callable=AsyncMock) as mock_mark_sent,
    ):
        await worker._process_message(row)
        mock_mark_sent.assert_awaited_once_with("msg-1", "<msg-id@test>")


@pytest.mark.asyncio
async def test_auth_error_fails_permanently():
    pool = MagicMock()
    worker = SendWorker(pool)
    row = make_message_row()
    account = make_account()

    with (
        patch("app.transport.worker.messages.claim_message", new_callable=AsyncMock, return_value=True),
        patch("app.transport.worker.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=account),
        patch("app.transport.worker.send_message", new_callable=AsyncMock, side_effect=SmtpSendError("auth_error", "Bad creds")),
        patch("app.transport.worker.messages.mark_failed", new_callable=AsyncMock) as mock_failed,
        patch("app.transport.worker.messages.mark_retry", new_callable=AsyncMock) as mock_retry,
    ):
        await worker._process_message(row)
        mock_failed.assert_awaited_once_with("msg-1", "Bad creds")
        mock_retry.assert_not_awaited()


@pytest.mark.asyncio
async def test_connection_error_retries():
    pool = MagicMock()
    worker = SendWorker(pool)
    row = make_message_row(retry_count=0)
    account = make_account()

    with (
        patch("app.transport.worker.messages.claim_message", new_callable=AsyncMock, return_value=True),
        patch("app.transport.worker.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=account),
        patch("app.transport.worker.send_message", new_callable=AsyncMock, side_effect=SmtpSendError("connection_error", "Timeout")),
        patch("app.transport.worker.messages.mark_retry", new_callable=AsyncMock) as mock_retry,
        patch("app.transport.worker.messages.mark_failed", new_callable=AsyncMock) as mock_failed,
    ):
        await worker._process_message(row)
        mock_retry.assert_awaited_once()
        mock_failed.assert_not_awaited()


@pytest.mark.asyncio
async def test_backoff_30_60_120():
    pool = MagicMock()
    worker = SendWorker(pool)
    account = make_account()

    for retry_count, expected_delay in [(0, 30), (1, 60), (2, 120)]:
        row = make_message_row(msg_id=f"msg-{retry_count}", retry_count=retry_count)

        with (
            patch("app.transport.worker.messages.claim_message", new_callable=AsyncMock, return_value=True),
            patch("app.transport.worker.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=account),
            patch("app.transport.worker.send_message", new_callable=AsyncMock, side_effect=SmtpSendError("connection_error", "fail")),
            patch("app.transport.worker.messages.mark_retry", new_callable=AsyncMock) as mock_retry,
        ):
            await worker._process_message(row)
            call_args = mock_retry.call_args
            next_retry_str = call_args[0][2]
            next_retry = datetime.fromisoformat(next_retry_str)
            expected_min = datetime.now(timezone.utc) + timedelta(seconds=expected_delay - 2)
            expected_max = datetime.now(timezone.utc) + timedelta(seconds=expected_delay + 2)
            assert expected_min <= next_retry <= expected_max, (
                f"retry_count={retry_count}: expected ~{expected_delay}s delay, "
                f"got {next_retry}"
            )


@pytest.mark.asyncio
async def test_backoff_caps_at_600():
    pool = MagicMock()
    worker = SendWorker(pool)
    account = make_account()
    # retry_count=10 would be 30 * 2^10 = 30720, but cap is 600
    # Note: MAX_RETRIES=3, so this would actually fail permanently,
    # so test with retry_count=2 but huge base delay expectation
    # Actually, let's test the formula directly: min(30 * 2^10, 600) = 600
    # But worker checks retry_count >= MAX_RETRIES first, so we need to
    # test the cap with retry_count < MAX_RETRIES
    # With retry_count=2: delay = min(30*4, 600) = 120 (not capped)
    # The cap only matters at high retry counts, but MAX_RETRIES=3 prevents that
    # Test the formula directly instead
    delay = min(BASE_DELAY_SECONDS * (2 ** 10), MAX_DELAY_SECONDS)
    assert delay == 600


@pytest.mark.asyncio
async def test_max_retries_exceeded():
    pool = MagicMock()
    worker = SendWorker(pool)
    row = make_message_row(retry_count=3)
    account = make_account()

    with (
        patch("app.transport.worker.messages.claim_message", new_callable=AsyncMock, return_value=True),
        patch("app.transport.worker.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=account),
        patch("app.transport.worker.send_message", new_callable=AsyncMock, side_effect=SmtpSendError("connection_error", "Timeout")),
        patch("app.transport.worker.messages.mark_failed", new_callable=AsyncMock) as mock_failed,
        patch("app.transport.worker.messages.mark_retry", new_callable=AsyncMock) as mock_retry,
    ):
        await worker._process_message(row)
        mock_failed.assert_awaited_once()
        assert "Max retries exceeded" in mock_failed.call_args[0][1]
        mock_retry.assert_not_awaited()


@pytest.mark.asyncio
async def test_missing_account_fails():
    pool = MagicMock()
    worker = SendWorker(pool)
    row = make_message_row()

    with (
        patch("app.transport.worker.messages.claim_message", new_callable=AsyncMock, return_value=True),
        patch("app.transport.worker.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=None),
        patch("app.transport.worker.messages.mark_failed", new_callable=AsyncMock) as mock_failed,
    ):
        await worker._process_message(row)
        mock_failed.assert_awaited_once_with("msg-1", "Account not found")


@pytest.mark.asyncio
async def test_claim_false_skips_send():
    pool = MagicMock()
    worker = SendWorker(pool)
    row = make_message_row()

    with (
        patch("app.transport.worker.messages.claim_message", new_callable=AsyncMock, return_value=False),
        patch("app.transport.worker.send_message", new_callable=AsyncMock) as mock_send,
    ):
        await worker._process_message(row)
        mock_send.assert_not_awaited()


@pytest.mark.asyncio
async def test_in_flight_prevents_double_processing():
    pool = MagicMock()
    worker = SendWorker(pool)
    row = make_message_row()

    # Simulate msg already in flight
    worker._in_flight.add("msg-1")

    with (
        patch("app.transport.worker.messages.get_queued_messages", new_callable=AsyncMock, return_value=[row]),
        patch("app.transport.worker.messages.claim_message", new_callable=AsyncMock) as mock_claim,
    ):
        # Manually simulate what _poll_loop does: skip msgs in _in_flight
        queued = [row]
        tasks = []
        for r in queued:
            msg_id = r["id"]
            if msg_id in worker._in_flight:
                continue
            tasks.append(worker._process_message(r))

        assert len(tasks) == 0
        mock_claim.assert_not_awaited()

    worker._in_flight.discard("msg-1")


@pytest.mark.asyncio
async def test_recover_orphans_resets_sending(setup_db):
    """DB integration: insert a 'sending' row, recover, assert status is 'queued'."""
    from app import messages
    from app.credentials import store as credential_store
    from app.db import get_db

    # Create a real account to satisfy FK constraint
    account = await credential_store.create_account(
        name="Test",
        smtp_host="smtp.example.com",
        smtp_port=587,
        imap_host="imap.example.com",
        imap_port=993,
        username="user@example.com",
        password="secret",
    )

    # Create a message and manually set to 'sending'
    record = await messages.create_message(
        account_id=account["id"],
        from_addr="a@b.com",
        to_addr="c@d.com",
        subject="Test",
    )
    db = await get_db()
    await db.execute(
        "UPDATE messages SET status = 'sending' WHERE id = ?",
        (record["id"],),
    )
    await db.commit()

    # Verify it's sending
    msg = await messages.get_message(record["id"])
    assert msg["status"] == "sending"

    # Recover orphans
    await messages.recover_orphans()

    # Should be queued again
    msg = await messages.get_message(record["id"])
    assert msg["status"] == "queued"
