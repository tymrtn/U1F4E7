# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import logging

from app import drafts, messages
from app.credentials import store as credential_store
from app.transport.smtp import build_mime_message, send_message, SmtpSendError

logger = logging.getLogger(__name__)

POLL_INTERVAL_SECONDS = 60
MAX_SEND_FAILURES = 3


class DraftScheduler:
    """Background worker that sends drafts whose send_after time has passed."""

    def __init__(self, pool):
        self._pool = pool
        self._task: asyncio.Task | None = None
        self._stopping = False

    async def start(self):
        self._stopping = False
        self._task = asyncio.ensure_future(self._poll_loop())
        logger.info("DraftScheduler started (polling every %ds)", POLL_INTERVAL_SECONDS)

    async def stop(self):
        self._stopping = True
        if self._task and not self._task.done():
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
        logger.info("DraftScheduler stopped")

    async def _poll_loop(self):
        while not self._stopping:
            try:
                due_drafts = await drafts.get_scheduled_drafts()
                for draft in due_drafts:
                    if self._stopping:
                        break
                    await self._send_draft(draft)
            except asyncio.CancelledError:
                raise
            except Exception:
                logger.exception("DraftScheduler poll error")

            try:
                await asyncio.sleep(POLL_INTERVAL_SECONDS)
            except asyncio.CancelledError:
                raise

    async def _send_draft(self, draft: dict):
        draft_id = draft["id"]
        account_id = draft["account_id"]

        account = await credential_store.get_account_with_credentials(account_id)
        if not account:
            logger.error("Draft %s: account %s not found, marking failed", draft_id[:8], account_id[:8])
            meta = draft.get("metadata") or {}
            meta["scheduler_error"] = "Account not found"
            await drafts.update_draft(draft_id, metadata=meta)
            await drafts.discard_draft(draft_id)
            return

        from_addr = account["username"]

        msg = build_mime_message(
            from_addr=from_addr,
            to_addr=draft["to_addr"],
            subject=draft["subject"] or "",
            text=draft["text_content"],
            html=draft["html_content"],
            display_name=account.get("display_name"),
        )
        if draft.get("in_reply_to"):
            msg["In-Reply-To"] = draft["in_reply_to"]

        record = await messages.create_message(
            account_id=account_id,
            from_addr=from_addr,
            to_addr=draft["to_addr"],
            subject=draft["subject"],
            text_content=draft["text_content"],
            html_content=draft["html_content"],
        )

        try:
            smtp_message_id = await send_message(account, msg, pool=self._pool)
        except SmtpSendError as e:
            await messages.mark_failed(record["id"], e.message)
            logger.error("Draft %s: SMTP error: %s", draft_id[:8], e.message)

            meta = draft.get("metadata") or {}
            failures = meta.get("send_failures", 0) + 1
            meta["send_failures"] = failures
            meta["last_error"] = e.message

            if failures >= MAX_SEND_FAILURES:
                await drafts.update_draft(draft_id, metadata=meta)
                await drafts.discard_draft(draft_id)
                logger.error("Draft %s: max failures reached, discarded", draft_id[:8])
            else:
                await drafts.update_draft(draft_id, metadata=meta)
            return

        await messages.mark_sent(record["id"], smtp_message_id)
        await drafts.mark_draft_sent(draft_id, record["id"])
        logger.info("DraftScheduler sent draft %s to %s", draft_id[:8], draft["to_addr"])
