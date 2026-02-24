# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import time
import pytest
from unittest.mock import AsyncMock, MagicMock, patch

from app.transport.pool import SmtpConnectionPool, PoolConfig, PooledConnection


def make_account(port=587, account_id="acct-1"):
    return {
        "id": account_id,
        "smtp_host": "smtp.example.com",
        "smtp_port": port,
        "effective_smtp_username": "user@example.com",
        "effective_smtp_password": "secret",
    }


def make_mock_smtp():
    client = AsyncMock()
    client.connect = AsyncMock()
    client.login = AsyncMock()
    client.noop = AsyncMock()
    client.quit = AsyncMock()
    client.close = MagicMock()
    return client


@pytest.mark.asyncio
async def test_acquire_creates_connection():
    pool = SmtpConnectionPool()
    account = make_account()
    mock_client = make_mock_smtp()

    with patch("app.transport.pool.aiosmtplib.SMTP", return_value=mock_client):
        async with pool.acquire(account) as client:
            assert client is mock_client

    mock_client.connect.assert_awaited_once()
    mock_client.login.assert_awaited_once_with("user@example.com", "secret")
    await pool.close_all()


@pytest.mark.asyncio
async def test_acquire_reuses_idle_connection():
    pool = SmtpConnectionPool()
    account = make_account()
    mock_client = make_mock_smtp()

    with patch("app.transport.pool.aiosmtplib.SMTP", return_value=mock_client) as MockSMTP:
        async with pool.acquire(account) as client:
            pass

        # Second acquire should reuse (noop succeeds)
        async with pool.acquire(account) as client:
            assert client is mock_client

        assert MockSMTP.call_count == 1
        mock_client.noop.assert_awaited()

    await pool.close_all()


@pytest.mark.asyncio
async def test_acquire_discards_on_exception():
    pool = SmtpConnectionPool()
    account = make_account()
    mock_client = make_mock_smtp()

    with patch("app.transport.pool.aiosmtplib.SMTP", return_value=mock_client):
        with pytest.raises(ValueError):
            async with pool.acquire(account) as client:
                raise ValueError("boom")

    # Connection should have been closed, not returned to idle
    mock_client.quit.assert_awaited()
    assert pool._idle.get("acct-1", []) == []
    await pool.close_all()


@pytest.mark.asyncio
async def test_semaphore_blocks_at_capacity():
    config = PoolConfig(max_connections_per_account=1)
    pool = SmtpConnectionPool(config=config)
    account = make_account()
    mock_client = make_mock_smtp()

    with patch("app.transport.pool.aiosmtplib.SMTP", return_value=mock_client):
        async with pool.acquire(account):
            # Second acquire should block and timeout
            with pytest.raises(asyncio.TimeoutError):
                await asyncio.wait_for(
                    pool.acquire(account).__aenter__(), timeout=0.1
                )

    await pool.close_all()


@pytest.mark.asyncio
async def test_semaphore_released_on_error():
    config = PoolConfig(max_connections_per_account=1)
    pool = SmtpConnectionPool(config=config)
    account = make_account()
    mock_client = make_mock_smtp()
    mock_client.connect.side_effect = OSError("connect failed")

    with patch("app.transport.pool.aiosmtplib.SMTP", return_value=mock_client):
        with pytest.raises(OSError):
            async with pool.acquire(account):
                pass

        # Semaphore should be released — next acquire should not block
        mock_client.connect.side_effect = None
        async with pool.acquire(account) as client:
            assert client is mock_client

    await pool.close_all()


@pytest.mark.asyncio
async def test_invalidate_bumps_version_and_drains():
    pool = SmtpConnectionPool()
    account = make_account()
    mock_client = make_mock_smtp()

    with patch("app.transport.pool.aiosmtplib.SMTP", return_value=mock_client):
        async with pool.acquire(account):
            pass

        assert len(pool._idle.get("acct-1", [])) == 1

        pool.invalidate_account("acct-1")

        assert pool._credential_versions["acct-1"] == 1
        assert pool._idle.get("acct-1", []) == []
        # invalidate uses ensure_future — give event loop a tick
        await asyncio.sleep(0)
        mock_client.quit.assert_awaited()

    await pool.close_all()


@pytest.mark.asyncio
async def test_stale_credential_version_evicted():
    pool = SmtpConnectionPool()
    account = make_account()

    mock_client_v0 = make_mock_smtp()
    mock_client_v1 = make_mock_smtp()

    with patch("app.transport.pool.aiosmtplib.SMTP", side_effect=[mock_client_v0, mock_client_v1]):
        # First connection at version 0
        async with pool.acquire(account):
            pass

        # Bump version
        pool._credential_versions["acct-1"] = 1

        # Next acquire should evict stale v0 and create new
        async with pool.acquire(account) as client:
            assert client is mock_client_v1

    mock_client_v0.quit.assert_awaited()
    await pool.close_all()


@pytest.mark.asyncio
async def test_noop_failure_evicts_connection():
    pool = SmtpConnectionPool()
    account = make_account()

    mock_client_1 = make_mock_smtp()
    mock_client_1.noop.side_effect = OSError("dead")
    mock_client_2 = make_mock_smtp()

    with patch("app.transport.pool.aiosmtplib.SMTP", side_effect=[mock_client_1, mock_client_2]):
        async with pool.acquire(account):
            pass

        # Second acquire: noop fails on client_1 → creates client_2
        async with pool.acquire(account) as client:
            assert client is mock_client_2

    mock_client_1.quit.assert_awaited()
    await pool.close_all()


@pytest.mark.asyncio
async def test_idle_timeout_eviction():
    config = PoolConfig(max_idle_seconds=0.01)
    pool = SmtpConnectionPool(config=config)
    account = make_account()
    mock_client = make_mock_smtp()

    with patch("app.transport.pool.aiosmtplib.SMTP", return_value=mock_client):
        # Create and return a connection
        pc = PooledConnection(client=mock_client, returned_at=time.monotonic() - 1)
        pool._idle["acct-1"] = [pc]

        await pool._evict_stale()

    assert pool._idle.get("acct-1") is None
    mock_client.quit.assert_awaited()
    await pool.close_all()


@pytest.mark.asyncio
async def test_lifetime_eviction():
    config = PoolConfig(max_lifetime_seconds=0.01)
    pool = SmtpConnectionPool(config=config)
    mock_client = make_mock_smtp()

    pc = PooledConnection(client=mock_client, created_at=time.monotonic() - 1)
    pool._idle["acct-1"] = [pc]

    await pool._evict_stale()

    assert pool._idle.get("acct-1") is None
    mock_client.quit.assert_awaited()
    await pool.close_all()


@pytest.mark.asyncio
async def test_close_all_drains_and_cancels():
    pool = SmtpConnectionPool()
    mock_client = make_mock_smtp()

    pc = PooledConnection(client=mock_client)
    pool._idle["acct-1"] = [pc]

    # Start cleanup task
    pool.start_cleanup_task()
    assert pool._cleanup_task is not None
    assert not pool._cleanup_task.done()

    await pool.close_all()

    assert pool._idle.get("acct-1", []) == []
    assert pool._cleanup_task.done()
    mock_client.quit.assert_awaited()


@pytest.mark.asyncio
async def test_port_465_uses_tls():
    pool = SmtpConnectionPool()
    account = make_account(port=465)
    mock_client = make_mock_smtp()

    with patch("app.transport.pool.aiosmtplib.SMTP", return_value=mock_client) as MockSMTP:
        async with pool.acquire(account):
            pass

        MockSMTP.assert_called_once_with(
            hostname="smtp.example.com",
            port=465,
            use_tls=True,
            start_tls=False,
            timeout=30,
        )

    await pool.close_all()


@pytest.mark.asyncio
async def test_port_587_uses_starttls():
    pool = SmtpConnectionPool()
    account = make_account(port=587)
    mock_client = make_mock_smtp()

    with patch("app.transport.pool.aiosmtplib.SMTP", return_value=mock_client) as MockSMTP:
        async with pool.acquire(account):
            pass

        MockSMTP.assert_called_once_with(
            hostname="smtp.example.com",
            port=587,
            use_tls=False,
            start_tls=True,
            timeout=30,
        )

    await pool.close_all()
