# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import json
import uuid
from datetime import datetime, timezone
from typing import Optional

from app.db import get_db


async def create_draft(
    account_id: str,
    to_addr: str,
    subject: Optional[str] = None,
    text_content: Optional[str] = None,
    html_content: Optional[str] = None,
    in_reply_to: Optional[str] = None,
    metadata: Optional[dict] = None,
    created_by: Optional[str] = None,
    send_after: Optional[str] = None,
    snoozed_until: Optional[str] = None,
) -> dict:
    draft_id = str(uuid.uuid4())
    now = datetime.now(timezone.utc).isoformat()
    meta_json = json.dumps(metadata) if metadata else None

    db = await get_db()
    await db.execute(
        """INSERT INTO drafts
        (id, account_id, status, to_addr, subject, text_content, html_content,
         in_reply_to, metadata, created_at, updated_at, created_by,
         send_after, snoozed_until)
        VALUES (?, ?, 'draft', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
        (draft_id, account_id, to_addr, subject, text_content, html_content,
         in_reply_to, meta_json, now, now, created_by, send_after, snoozed_until),
    )
    await db.commit()

    return {
        "id": draft_id,
        "account_id": account_id,
        "status": "draft",
        "to_addr": to_addr,
        "subject": subject,
        "text_content": text_content,
        "html_content": html_content,
        "in_reply_to": in_reply_to,
        "metadata": metadata,
        "message_id": None,
        "created_at": now,
        "updated_at": now,
        "sent_at": None,
        "created_by": created_by,
        "send_after": send_after,
        "snoozed_until": snoozed_until,
    }


async def list_drafts(
    account_id: str,
    limit: int = 50,
    offset: int = 0,
    status: Optional[str] = None,
    created_by: Optional[str] = None,
    hide_snoozed: bool = False,
) -> list[dict]:
    db = await get_db()
    query = """SELECT id, account_id, status, to_addr, subject, text_content,
                  html_content, in_reply_to, metadata, message_id,
                  created_at, updated_at, sent_at, created_by,
                  send_after, snoozed_until
           FROM drafts
           WHERE account_id = ?"""
    params: list = [account_id]
    if status:
        query += " AND status = ?"
        params.append(status)
    if created_by:
        query += " AND created_by = ?"
        params.append(created_by)
    if hide_snoozed:
        query += " AND (snoozed_until IS NULL OR datetime(snoozed_until) <= datetime('now'))"
    query += " ORDER BY updated_at DESC LIMIT ? OFFSET ?"
    params.extend([limit, offset])
    cursor = await db.execute(query, params)
    rows = await cursor.fetchall()
    return [_row_to_dict(row) for row in rows]


async def get_draft(draft_id: str) -> Optional[dict]:
    db = await get_db()
    cursor = await db.execute(
        """SELECT id, account_id, status, to_addr, subject, text_content,
                  html_content, in_reply_to, metadata, message_id,
                  created_at, updated_at, sent_at, created_by,
                  send_after, snoozed_until
           FROM drafts WHERE id = ?""",
        (draft_id,),
    )
    row = await cursor.fetchone()
    return _row_to_dict(row) if row else None


async def update_draft(draft_id: str, **fields) -> Optional[dict]:
    draft = await get_draft(draft_id)
    if not draft:
        return None
    if draft["status"] != "draft":
        return None

    # Fields that can be explicitly set to NULL (clearing them)
    clearable_fields = {"send_after", "snoozed_until"}
    allowed = {"to_addr", "subject", "text_content", "html_content",
               "in_reply_to", "metadata", "send_after", "snoozed_until"}

    updates = {}
    for k, v in fields.items():
        if k not in allowed:
            continue
        if k in clearable_fields or v is not None:
            updates[k] = v

    if not updates:
        return draft

    now = datetime.now(timezone.utc).isoformat()
    if "metadata" in updates:
        updates["metadata"] = json.dumps(updates["metadata"])

    set_clause = ", ".join(f"{k} = ?" for k in updates)
    values = list(updates.values()) + [now, draft_id]

    db = await get_db()
    await db.execute(
        f"UPDATE drafts SET {set_clause}, updated_at = ? WHERE id = ?",
        values,
    )
    await db.commit()
    return await get_draft(draft_id)


async def discard_draft(draft_id: str) -> bool:
    db = await get_db()
    now = datetime.now(timezone.utc).isoformat()
    cursor = await db.execute(
        "UPDATE drafts SET status = 'discarded', updated_at = ? WHERE id = ? AND status = 'draft'",
        (now, draft_id),
    )
    await db.commit()
    return cursor.rowcount > 0


async def mark_draft_sent(draft_id: str, message_id: str) -> None:
    db = await get_db()
    now = datetime.now(timezone.utc).isoformat()
    await db.execute(
        "UPDATE drafts SET status = 'sent', message_id = ?, sent_at = ?, updated_at = ? WHERE id = ?",
        (message_id, now, now, draft_id),
    )
    await db.commit()


async def get_scheduled_drafts() -> list[dict]:
    """Return drafts that are approved (send_after set) and past their send time."""
    db = await get_db()
    cursor = await db.execute(
        """SELECT id, account_id, status, to_addr, subject, text_content,
                  html_content, in_reply_to, metadata, message_id,
                  created_at, updated_at, sent_at, created_by,
                  send_after, snoozed_until
           FROM drafts
           WHERE status = 'draft'
             AND send_after IS NOT NULL
             AND datetime(send_after) <= datetime('now')
           ORDER BY send_after ASC"""
    )
    rows = await cursor.fetchall()
    return [_row_to_dict(row) for row in rows]


def _row_to_dict(row) -> dict:
    meta_raw = row["metadata"]
    metadata = json.loads(meta_raw) if meta_raw else None
    return {
        "id": row["id"],
        "account_id": row["account_id"],
        "status": row["status"],
        "to_addr": row["to_addr"],
        "subject": row["subject"],
        "text_content": row["text_content"],
        "html_content": row["html_content"],
        "in_reply_to": row["in_reply_to"],
        "metadata": metadata,
        "message_id": row["message_id"],
        "created_at": row["created_at"],
        "updated_at": row["updated_at"],
        "sent_at": row["sent_at"],
        "created_by": row["created_by"],
        "send_after": row["send_after"],
        "snoozed_until": row["snoozed_until"],
    }
