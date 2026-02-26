# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import fnmatch
import hashlib
import hmac
import json
import logging
import os
from datetime import datetime, timezone

import httpx

from app.credentials.store import list_accounts, get_account_with_credentials
from app.db import get_db
from app.transport.imap import search_messages, fetch_message, ImapError

logger = logging.getLogger(__name__)


class WebhookPoller:
    def __init__(self):
        self._task: asyncio.Task | None = None

    async def start(self):
        self._task = asyncio.create_task(self._poll_loop())

    async def stop(self):
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
            self._task = None

    async def _poll_loop(self):
        interval = int(os.getenv("WEBHOOK_POLL_INTERVAL_SECONDS", "60"))
        while True:
            await asyncio.sleep(interval)
            try:
                accounts = await list_accounts()
                for account in accounts:
                    if account.get("webhook_url"):
                        try:
                            await self._check_account(account)
                        except Exception as e:
                            logger.error(f"Error checking account {account['id']}: {e}")
            except Exception as e:
                logger.error(f"Webhook poll loop error: {e}")

    async def _check_account(self, account: dict):
        account_id = account["id"]

        full_account = await get_account_with_credentials(account_id)
        if not full_account:
            return

        db = await get_db()
        cursor = await db.execute(
            "SELECT last_uid FROM webhook_state WHERE account_id = ?",
            (account_id,),
        )
        row = await cursor.fetchone()
        last_uid = int(row["last_uid"]) if row and row["last_uid"] else 0

        # Search for new messages using UID range (doesn't touch UNSEEN flags)
        try:
            query = f"UID {last_uid + 1}:*" if last_uid > 0 else "ALL"
            summaries = await search_messages(full_account, query=query)
        except ImapError as e:
            logger.warning(f"IMAP error for account {account_id}: {e.message}")
            return

        if not summaries:
            return

        # Get address policies for sender-specific webhook URL overrides
        from app.services.policy import list_address_policies
        address_policies = await list_address_policies(account_id)

        # Get decrypted webhook secret
        webhook_secret = full_account.get("webhook_secret")

        highest_uid = last_uid

        async with httpx.AsyncClient(timeout=10.0) as client:
            for summary in summaries:
                uid_str = summary.get("uid", "")
                try:
                    uid_int = int(uid_str)
                except (ValueError, TypeError):
                    continue

                if uid_int <= last_uid:
                    continue

                try:
                    msg = await fetch_message(full_account, folder="INBOX", uid=uid_str)
                except ImapError:
                    continue

                if not msg:
                    continue

                # Determine webhook URL: check address_policies for sender match first
                webhook_url = account["webhook_url"]
                sender = msg.get("from_addr", "")
                for policy in address_policies:
                    if policy.get("webhook_url") and fnmatch.fnmatch(sender, policy["pattern"]):
                        webhook_url = policy["webhook_url"]
                        break

                payload = {
                    "account_id": account_id,
                    "uid": uid_str,
                    "message_id": msg.get("message_id"),
                    "from_addr": msg.get("from_addr"),
                    "to_addr": msg.get("to_addr"),
                    "subject": msg.get("subject"),
                    "date": msg.get("date"),
                    "snippet": (msg.get("text_body") or "")[:200],
                    "has_html": bool(msg.get("html_body")),
                    "attachments": msg.get("attachments", []),
                }
                payload_bytes = json.dumps(payload).encode()

                headers = {"Content-Type": "application/json"}
                if webhook_secret:
                    headers["X-Envelope-Signature"] = self._sign(webhook_secret, payload_bytes)

                try:
                    resp = await client.post(
                        webhook_url,
                        content=payload_bytes,
                        headers=headers,
                    )
                    resp.raise_for_status()
                    logger.info(
                        f"Webhook delivered for account {account_id} uid {uid_str}: {resp.status_code}"
                    )
                except Exception as e:
                    logger.warning(
                        f"Webhook delivery failed for account {account_id} uid {uid_str}: {e}"
                    )

                highest_uid = max(highest_uid, uid_int)

        if highest_uid > last_uid:
            now = datetime.now(timezone.utc).isoformat()
            await db.execute(
                """INSERT OR REPLACE INTO webhook_state (account_id, last_uid, updated_at)
                   VALUES (?, ?, ?)""",
                (account_id, str(highest_uid), now),
            )
            await db.commit()

    def _sign(self, secret: str, payload_bytes: bytes) -> str:
        return hmac.new(secret.encode(), payload_bytes, hashlib.sha256).hexdigest()
