# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import logging
import time
from contextlib import asynccontextmanager
from dataclasses import dataclass, field

import aiosmtplib

logger = logging.getLogger(__name__)


@dataclass
class PoolConfig:
    max_connections_per_account: int = 2
    max_idle_seconds: float = 270
    max_lifetime_seconds: float = 3600
    cleanup_interval_seconds: float = 60
    noop_check_before_use: bool = True


@dataclass
class PooledConnection:
    client: aiosmtplib.SMTP
    created_at: float = field(default_factory=time.monotonic)
    returned_at: float = field(default_factory=time.monotonic)
    credential_version: int = 0


class SmtpConnectionPool:
    def __init__(self, config: PoolConfig | None = None):
        self._config = config or PoolConfig()
        self._idle: dict[str, list[PooledConnection]] = {}
        self._credential_versions: dict[str, int] = {}
        self._semaphores: dict[str, asyncio.Semaphore] = {}
        self._cleanup_task: asyncio.Task | None = None

    def _get_semaphore(self, account_id: str) -> asyncio.Semaphore:
        if account_id not in self._semaphores:
            self._semaphores[account_id] = asyncio.Semaphore(
                self._config.max_connections_per_account
            )
        return self._semaphores[account_id]

    def _credential_version(self, account_id: str) -> int:
        return self._credential_versions.get(account_id, 0)

    def invalidate_account(self, account_id: str):
        self._credential_versions[account_id] = (
            self._credential_versions.get(account_id, 0) + 1
        )
        stale = self._idle.pop(account_id, [])
        for conn in stale:
            asyncio.ensure_future(self._close_connection(conn))
        logger.info("Invalidated pool for account %s", account_id)

    @asynccontextmanager
    async def acquire(self, account: dict):
        account_id = account["id"]
        sem = self._get_semaphore(account_id)
        await sem.acquire()
        conn = None
        try:
            conn = await self._get_or_create(account)
            yield conn.client
            # Success — return to idle pool
            conn.returned_at = time.monotonic()
            self._idle.setdefault(account_id, []).append(conn)
            conn = None  # prevent finally from closing it
        except Exception:
            raise
        finally:
            if conn is not None:
                await self._close_connection(conn)
            sem.release()

    async def _get_or_create(self, account: dict) -> PooledConnection:
        account_id = account["id"]
        now = time.monotonic()
        current_version = self._credential_version(account_id)
        idle_list = self._idle.get(account_id, [])

        while idle_list:
            candidate = idle_list.pop()
            # Evict if credentials changed
            if candidate.credential_version != current_version:
                await self._close_connection(candidate)
                continue
            # Evict if too old
            if now - candidate.created_at > self._config.max_lifetime_seconds:
                await self._close_connection(candidate)
                continue
            # Evict if idle too long
            if now - candidate.returned_at > self._config.max_idle_seconds:
                await self._close_connection(candidate)
                continue
            # NOOP check — verify connection is still alive
            if self._config.noop_check_before_use:
                try:
                    await candidate.client.noop()
                    logger.debug("Reusing pooled connection for %s", account_id)
                    return candidate
                except Exception:
                    await self._close_connection(candidate)
                    continue
            logger.debug("Reusing pooled connection for %s", account_id)
            return candidate

        # No reusable connection — create fresh
        return await self._create_connection(account, current_version)

    async def _create_connection(
        self, account: dict, credential_version: int
    ) -> PooledConnection:
        port = account["smtp_port"]
        client = aiosmtplib.SMTP(
            hostname=account["smtp_host"],
            port=port,
            use_tls=(port == 465),
            start_tls=(port != 465),
            timeout=30,
        )
        await client.connect()
        await client.login(
            account["effective_smtp_username"],
            account["effective_smtp_password"],
        )
        logger.debug("Created new SMTP connection for %s", account["id"])
        return PooledConnection(
            client=client,
            credential_version=credential_version,
        )

    async def _close_connection(self, conn: PooledConnection):
        try:
            await conn.client.quit()
        except Exception:
            try:
                conn.client.close()
            except Exception:
                pass

    def start_cleanup_task(self):
        if self._cleanup_task is None or self._cleanup_task.done():
            self._cleanup_task = asyncio.ensure_future(self._cleanup_loop())

    async def _cleanup_loop(self):
        while True:
            await asyncio.sleep(self._config.cleanup_interval_seconds)
            await self._evict_stale()

    async def _evict_stale(self):
        now = time.monotonic()
        for account_id in list(self._idle.keys()):
            idle_list = self._idle.get(account_id, [])
            keep = []
            for conn in idle_list:
                if (
                    now - conn.returned_at > self._config.max_idle_seconds
                    or now - conn.created_at > self._config.max_lifetime_seconds
                ):
                    await self._close_connection(conn)
                else:
                    keep.append(conn)
            if keep:
                self._idle[account_id] = keep
            else:
                self._idle.pop(account_id, None)

    async def close_all(self):
        if self._cleanup_task and not self._cleanup_task.done():
            self._cleanup_task.cancel()
            try:
                await self._cleanup_task
            except asyncio.CancelledError:
                pass

        for account_id in list(self._idle.keys()):
            for conn in self._idle.pop(account_id, []):
                await self._close_connection(conn)
