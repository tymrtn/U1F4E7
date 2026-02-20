# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import logging
from datetime import datetime, timezone, timedelta

from app import messages
from app.credentials import store as credential_store
from app.transport.smtp import build_mime_message, send_message, SmtpSendError

logger = logging.getLogger(__name__)

MAX_RETRIES = 3
BASE_DELAY_SECONDS = 30
MAX_DELAY_SECONDS = 600  # 10 minutes


class SendWorker:
    def __init__(self, pool):
        self._pool = pool
        self._semaphore = asyncio.Semaphore(5)
        self._work_available = asyncio.Event()
        self._in_flight: set[str] = set()
        self._task: asyncio.Task | None = None
        self._stopping = False

    async def start(self):
        await messages.recover_orphans()
        self._stopping = False
        self._task = asyncio.ensure_future(self._poll_loop())
        logger.info("SendWorker started")

    async def stop(self):
        self._stopping = True
        self._work_available.set()  # Wake the loop so it sees _stopping
        if self._task and not self._task.done():
            # Wait for in-flight sends to drain (up to 30s)
            for _ in range(60):
                if not self._in_flight:
                    break
                await asyncio.sleep(0.5)
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
        logger.info("SendWorker stopped")

    def notify(self):
        self._work_available.set()

    async def _poll_loop(self):
        while not self._stopping:
            try:
                queued = await messages.get_queued_messages(limit=10)
                if not queued:
                    self._work_available.clear()
                    try:
                        await asyncio.wait_for(
                            self._work_available.wait(), timeout=5.0
                        )
                    except asyncio.TimeoutError:
                        pass
                    continue

                tasks = []
                for row in queued:
                    msg_id = row["id"]
                    if msg_id in self._in_flight:
                        continue
                    self._in_flight.add(msg_id)
                    tasks.append(self._process_message(row))

                if tasks:
                    await asyncio.gather(*tasks, return_exceptions=True)

            except asyncio.CancelledError:
                raise
            except Exception:
                logger.exception("SendWorker poll error")
                await asyncio.sleep(5)

    async def _process_message(self, row: dict):
        msg_id = row["id"]
        try:
            async with self._semaphore:
                claimed = await messages.claim_message(msg_id)
                if not claimed:
                    return

                account = await credential_store.get_account_with_credentials(
                    row["account_id"]
                )
                if not account:
                    await messages.mark_failed(msg_id, "Account not found")
                    return

                from_addr = row["from_addr"]
                mime_msg = build_mime_message(
                    from_addr=from_addr,
                    to_addr=row["to_addr"],
                    subject=row["subject"],
                    text=row.get("text_content"),
                    html=row.get("html_content"),
                    display_name=account.get("display_name"),
                )

                try:
                    smtp_message_id = await send_message(
                        account, mime_msg, pool=self._pool
                    )
                    await messages.mark_sent(msg_id, smtp_message_id)
                    logger.info("Sent message %s", msg_id[:8])
                except SmtpSendError as e:
                    await self._handle_send_error(msg_id, row, e)

        except Exception:
            logger.exception("Failed to process message %s", msg_id[:8])
            try:
                await messages.mark_failed(msg_id, "Internal worker error")
            except Exception:
                pass
        finally:
            self._in_flight.discard(msg_id)

    async def _handle_send_error(self, msg_id: str, row: dict, error: SmtpSendError):
        # Auth errors are permanent — no retry
        if error.error_type == "auth_error":
            await messages.mark_failed(msg_id, error.message)
            return

        retry_count = row.get("retry_count", 0)
        if retry_count >= MAX_RETRIES:
            await messages.mark_failed(
                msg_id, f"Max retries exceeded: {error.message}"
            )
            return

        delay = min(BASE_DELAY_SECONDS * (2 ** retry_count), MAX_DELAY_SECONDS)
        next_retry = datetime.now(timezone.utc) + timedelta(seconds=delay)
        await messages.mark_retry(msg_id, error.message, next_retry.isoformat())
        logger.info(
            "Message %s scheduled for retry in %ds (attempt %d/%d)",
            msg_id[:8], delay, retry_count + 1, MAX_RETRIES,
        )
