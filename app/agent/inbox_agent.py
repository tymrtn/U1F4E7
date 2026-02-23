# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import json
import logging
import os
import uuid
from datetime import datetime, timezone
from typing import Optional

from app.agent.llm import chat_completion
from app.agent.prompts import (
    CLASSIFIER_SYSTEM_PROMPT,
    CLASSIFIER_USER_TEMPLATE,
    CLASSIFIER_USER_TEMPLATE_WITH_CONTEXT,
    CLASSIFIER_USER_TEMPLATE_WITH_SEMANTIC,
    CLASSIFIER_USER_TEMPLATE_FULL,
)
from app.credentials import store as credential_store
from app.db import get_db
from app import drafts
from app.transport.imap import InboundMessage, fetch_unread, mark_seen, get_thread
from app.transport.smtp import build_mime_message, send_message

logger = logging.getLogger(__name__)

DEFAULT_POLL_INTERVAL = 120


class InboxAgent:
    def __init__(self, smtp_pool, config: Optional[dict] = None):
        self._pool = smtp_pool
        self._config = config or {}
        self._task: asyncio.Task | None = None
        self._stopping = False
        self._last_poll: Optional[str] = None
        self._poll_count = 0
        self._action_counts = {"auto_reply": 0, "draft_for_review": 0, "escalate": 0, "ignore": 0}

    @property
    def account_id(self) -> str:
        return self._config.get("account_id") or os.getenv("AGENT_ACCOUNT_ID", "")

    @property
    def poll_interval(self) -> int:
        return int(
            self._config.get("poll_interval")
            or os.getenv("AGENT_POLL_INTERVAL", str(DEFAULT_POLL_INTERVAL))
        )

    @property
    def escalation_email(self) -> str:
        return (
            self._config.get("escalation_email")
            or os.getenv("AGENT_ESCALATION_EMAIL", "")
        )

    @property
    def send_from(self) -> str:
        return (
            self._config.get("send_from")
            or os.getenv("AGENT_SEND_FROM", "")
        )

    async def start(self):
        self._stopping = False
        self._task = asyncio.ensure_future(self._poll_loop())
        logger.info("InboxAgent started (poll every %ds)", self.poll_interval)

    async def stop(self):
        self._stopping = True
        if self._task and not self._task.done():
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
        logger.info("InboxAgent stopped")

    def status(self) -> dict:
        return {
            "running": self._task is not None and not self._task.done(),
            "last_poll": self._last_poll,
            "poll_count": self._poll_count,
            "poll_interval": self.poll_interval,
            "action_counts": dict(self._action_counts),
        }

    async def poll_once(self) -> list[dict]:
        return await self._do_poll()

    async def _poll_loop(self):
        while not self._stopping:
            try:
                await self._do_poll()
            except asyncio.CancelledError:
                raise
            except Exception:
                logger.exception("InboxAgent poll error")

            try:
                await asyncio.sleep(self.poll_interval)
            except asyncio.CancelledError:
                raise

    async def _do_poll(self) -> list[dict]:
        results = []
        account_id = self.account_id
        if not account_id:
            logger.warning("InboxAgent: no AGENT_ACCOUNT_ID configured")
            return results

        account = await credential_store.get_account_with_credentials(account_id)
        if not account:
            logger.error("InboxAgent: account %s not found", account_id[:8])
            return results

        self._last_poll = datetime.now(timezone.utc).isoformat()
        self._poll_count += 1

        try:
            unread = await fetch_unread(account)
        except Exception:
            logger.exception("InboxAgent: IMAP fetch failed")
            return results

        logger.info("InboxAgent: found %d unread messages", len(unread))

        for msg in unread:
            if await self._already_processed(msg):
                continue
            try:
                result = await self._process_message(account, msg)
                results.append(result)
            except Exception:
                logger.exception("InboxAgent: failed to process message uid=%s", msg.uid)

        return results

    async def _already_processed(self, msg: InboundMessage) -> bool:
        if not msg.message_id:
            return False
        db = await get_db()
        cursor = await db.execute(
            "SELECT 1 FROM agent_actions WHERE inbound_message_id = ?",
            (msg.message_id,),
        )
        return await cursor.fetchone() is not None

    async def _process_message(self, account: dict, msg: InboundMessage) -> dict:
        body = msg.text_body or msg.html_body or ""

        # Fetch thread context if this is a reply
        thread_context = ""
        if msg.in_reply_to or msg.references:
            thread_context = await self._fetch_thread_context(account, msg)

        # Fetch semantic context from embeddings
        semantic_context = await self._fetch_semantic_context(account, msg)

        # Build prompt with available context
        user_prompt = self._build_classifier_prompt(
            msg, body, thread_context, semantic_context,
        )

        llm_resp = await chat_completion(
            system_prompt=CLASSIFIER_SYSTEM_PROMPT,
            user_message=user_prompt,
        )

        parsed = self._parse_llm_response(llm_resp.content)
        classification = parsed.get("classification", "escalate")
        confidence = parsed.get("confidence", 0.0)
        action = classification
        draft_reply = parsed.get("draft_reply")
        escalation_note = parsed.get("escalation_note")
        reasoning = parsed.get("reasoning", "")
        signals = parsed.get("signals", {})

        outbound_message_id = None

        if action == "auto_reply" and draft_reply:
            outbound_message_id = await self._send_reply(
                account, msg, draft_reply
            )
            await self._mark_seen_safe(account, msg.uid)

        elif action == "draft_for_review" and draft_reply:
            await self._create_review_draft(
                account, msg, draft_reply,
                classification=classification,
                confidence=confidence,
                reasoning=reasoning,
                signals=signals,
            )
            await self._mark_seen_safe(account, msg.uid)

        elif action == "escalate":
            await self._create_escalation_draft(
                account, msg, escalation_note or reasoning,
                confidence=confidence,
                signals=signals,
            )
            await self._mark_seen_safe(account, msg.uid)

        elif action == "ignore":
            await self._mark_seen_safe(account, msg.uid)

        self._action_counts[action] = self._action_counts.get(action, 0) + 1

        record = await self._record_action(
            msg=msg,
            classification=classification,
            confidence=confidence,
            action=action,
            reasoning=reasoning,
            draft_reply=draft_reply,
            escalation_note=escalation_note,
            outbound_message_id=outbound_message_id,
        )

        logger.info(
            "InboxAgent: %s (%.2f) — %s from %s",
            action, confidence, msg.subject[:40], msg.from_addr,
        )
        return record

    def _build_classifier_prompt(
        self,
        msg: InboundMessage,
        body: str,
        thread_context: str,
        semantic_context: str,
    ) -> str:
        base_args = {
            "from_addr": msg.from_addr,
            "subject": msg.subject,
            "date": msg.date or "unknown",
            "body": body[:4000],
        }
        if thread_context and semantic_context:
            return CLASSIFIER_USER_TEMPLATE_FULL.format(
                **base_args,
                thread_context=thread_context,
                semantic_context=semantic_context,
            )
        elif thread_context:
            return CLASSIFIER_USER_TEMPLATE_WITH_CONTEXT.format(
                **base_args, thread_context=thread_context,
            )
        elif semantic_context:
            return CLASSIFIER_USER_TEMPLATE_WITH_SEMANTIC.format(
                **base_args, semantic_context=semantic_context,
            )
        return CLASSIFIER_USER_TEMPLATE.format(**base_args)

    async def _fetch_thread_context(
        self, account: dict, msg: InboundMessage
    ) -> str:
        target_id = msg.in_reply_to or msg.message_id
        if not target_id:
            return ""
        try:
            thread = await get_thread(account, target_id)
            if not thread:
                return ""
            parts = []
            for m in thread:
                if m.get("message_id") == msg.message_id:
                    continue  # skip the current message
                parts.append(
                    f"From: {m['from_addr']}\n"
                    f"Date: {m.get('date', 'unknown')}\n"
                    f"{(m.get('text_body') or '')[:1000]}\n"
                )
            return "\n---\n".join(parts) if parts else ""
        except Exception:
            logger.debug("InboxAgent: thread fetch failed, continuing without context")
            return ""

    async def _fetch_semantic_context(
        self, account: dict, msg: InboundMessage
    ) -> str:
        try:
            from app.agent.embeddings import find_similar
            query = f"{msg.subject} {(msg.text_body or '')[:500]}"
            results = await find_similar(account["id"], query, limit=3)
            if not results:
                return ""
            parts = []
            for r in results:
                parts.append(
                    f"Subject: {r['subject']} (from {r['from_addr']}, {r['date']})\n"
                    f"Relevance: {r['score']:.2f}\n"
                    f"{r.get('preview', '')[:500]}\n"
                )
            return "\n---\n".join(parts)
        except Exception:
            logger.debug("InboxAgent: semantic context fetch failed, continuing without")
            return ""

    def _parse_llm_response(self, content: str) -> dict:
        content = content.strip()
        if content.startswith("```"):
            lines = content.split("\n")
            lines = [l for l in lines if not l.strip().startswith("```")]
            content = "\n".join(lines)
        try:
            return json.loads(content)
        except json.JSONDecodeError:
            logger.warning("InboxAgent: failed to parse LLM response, escalating")
            return {
                "classification": "escalate",
                "confidence": 0.0,
                "reasoning": "Failed to parse LLM response",
                "draft_reply": None,
                "escalation_note": "LLM response was not valid JSON. Manual review needed.",
            }

    async def _send_reply(
        self, account: dict, inbound: InboundMessage, reply_text: str
    ) -> Optional[str]:
        from_addr = self.send_from or account["username"]
        to_addr = self._extract_email(inbound.from_addr)
        msg = build_mime_message(
            from_addr=from_addr,
            to_addr=to_addr,
            subject=f"Re: {inbound.subject}",
            text=reply_text,
            display_name=account.get("display_name"),
        )
        if inbound.message_id:
            msg["In-Reply-To"] = inbound.message_id
            msg["References"] = inbound.message_id

        try:
            return await send_message(account, msg, pool=self._pool)
        except Exception:
            logger.exception("InboxAgent: failed to send auto-reply")
            return None

    async def _create_review_draft(
        self,
        account: dict,
        inbound: InboundMessage,
        draft_text: str,
        classification: str = "draft_for_review",
        confidence: float = 0.0,
        reasoning: str = "",
        signals: Optional[dict] = None,
    ) -> Optional[dict]:
        try:
            return await drafts.create_draft(
                account_id=account["id"],
                to_addr=self._extract_email(inbound.from_addr),
                subject=f"Re: {inbound.subject}",
                text_content=draft_text,
                in_reply_to=inbound.message_id,
                metadata={
                    "agent": "inbox-agent",
                    "classification": classification,
                    "confidence": confidence,
                    "reasoning": reasoning,
                    "signals": signals or {},
                    "inbound_message_id": inbound.message_id,
                    "inbound_from": inbound.from_addr,
                    "inbound_subject": inbound.subject,
                    "inbound_date": inbound.date,
                    "inbound_preview": (inbound.text_body or "")[:500],
                },
                created_by="inbox-agent",
            )
        except Exception:
            logger.exception("InboxAgent: failed to create review draft")
            return None

    async def _create_escalation_draft(
        self,
        account: dict,
        inbound: InboundMessage,
        escalation_note: str,
        confidence: float = 0.0,
        signals: Optional[dict] = None,
    ) -> Optional[dict]:
        try:
            return await drafts.create_draft(
                account_id=account["id"],
                to_addr=self._extract_email(inbound.from_addr),
                subject=f"Re: {inbound.subject}",
                text_content=None,
                in_reply_to=inbound.message_id,
                metadata={
                    "agent": "inbox-agent",
                    "classification": "escalate",
                    "confidence": confidence,
                    "escalation_note": escalation_note,
                    "signals": signals or {},
                    "inbound_message_id": inbound.message_id,
                    "inbound_from": inbound.from_addr,
                    "inbound_subject": inbound.subject,
                    "inbound_date": inbound.date,
                    "inbound_preview": (inbound.text_body or "")[:500],
                },
                created_by="inbox-agent",
            )
        except Exception:
            logger.exception("InboxAgent: failed to create escalation draft")
            return None

    async def _mark_seen_safe(self, account: dict, uid: str) -> None:
        try:
            await mark_seen(account, uid)
        except Exception:
            logger.exception("InboxAgent: failed to mark uid=%s as seen", uid)

    async def _record_action(self, msg: InboundMessage, **kwargs) -> dict:
        action_id = str(uuid.uuid4())
        now = datetime.now(timezone.utc).isoformat()
        db = await get_db()
        await db.execute(
            """INSERT INTO agent_actions
            (id, inbound_message_id, from_addr, subject,
             classification, confidence, action, reasoning,
             draft_reply, escalation_note, outbound_message_id, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
            (
                action_id,
                msg.message_id or f"uid:{msg.uid}",
                msg.from_addr,
                msg.subject,
                kwargs.get("classification"),
                kwargs.get("confidence"),
                kwargs.get("action"),
                kwargs.get("reasoning"),
                kwargs.get("draft_reply"),
                kwargs.get("escalation_note"),
                kwargs.get("outbound_message_id"),
                now,
            ),
        )
        await db.commit()
        return {
            "id": action_id,
            "inbound_message_id": msg.message_id,
            "from_addr": msg.from_addr,
            "subject": msg.subject,
            "classification": kwargs.get("classification"),
            "confidence": kwargs.get("confidence"),
            "action": kwargs.get("action"),
            "created_at": now,
        }

    @staticmethod
    def _extract_email(addr: str) -> str:
        if "<" in addr and ">" in addr:
            return addr.split("<")[1].split(">")[0]
        return addr.strip()
