import uuid
from datetime import datetime, timezone
from typing import Optional

from app.db import get_db


async def create_message(
    account_id: str,
    from_addr: str,
    to_addr: str,
    subject: Optional[str] = None,
    direction: str = "outbound",
) -> dict:
    msg_id = str(uuid.uuid4())
    now = datetime.now(timezone.utc).isoformat()

    db = await get_db()
    try:
        await db.execute(
            """INSERT INTO messages
            (id, account_id, direction, from_addr, to_addr, subject, status, created_at)
            VALUES (?, ?, ?, ?, ?, ?, 'queued', ?)""",
            (msg_id, account_id, direction, from_addr, to_addr, subject, now),
        )
        await db.commit()
    finally:
        await db.close()

    return {
        "id": msg_id,
        "account_id": account_id,
        "direction": direction,
        "from_addr": from_addr,
        "to_addr": to_addr,
        "subject": subject,
        "status": "queued",
        "created_at": now,
    }


async def mark_sent(msg_id: str, message_id: str):
    now = datetime.now(timezone.utc).isoformat()
    db = await get_db()
    try:
        await db.execute(
            "UPDATE messages SET status = 'sent', message_id = ?, sent_at = ? WHERE id = ?",
            (message_id, now, msg_id),
        )
        await db.commit()
    finally:
        await db.close()


async def mark_failed(msg_id: str, error: str):
    db = await get_db()
    try:
        await db.execute(
            "UPDATE messages SET status = 'failed', error = ? WHERE id = ?",
            (error, msg_id),
        )
        await db.commit()
    finally:
        await db.close()


async def list_messages(limit: int = 50, offset: int = 0) -> list[dict]:
    db = await get_db()
    try:
        cursor = await db.execute(
            """SELECT id, account_id, message_id, direction, from_addr, to_addr,
                      subject, status, error, created_at, sent_at
               FROM messages ORDER BY created_at DESC LIMIT ? OFFSET ?""",
            (limit, offset),
        )
        rows = await cursor.fetchall()
        return [_row_to_dict(row) for row in rows]
    finally:
        await db.close()


async def get_message(msg_id: str) -> Optional[dict]:
    db = await get_db()
    try:
        cursor = await db.execute(
            """SELECT id, account_id, message_id, direction, from_addr, to_addr,
                      subject, status, error, created_at, sent_at
               FROM messages WHERE id = ?""",
            (msg_id,),
        )
        row = await cursor.fetchone()
        return _row_to_dict(row) if row else None
    finally:
        await db.close()


async def get_stats() -> dict:
    db = await get_db()
    try:
        cursor = await db.execute(
            """SELECT
                COUNT(*) as total,
                SUM(CASE WHEN status = 'sent' THEN 1 ELSE 0 END) as sent,
                SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END) as failed,
                SUM(CASE WHEN status = 'queued' THEN 1 ELSE 0 END) as queued
               FROM messages WHERE direction = 'outbound'"""
        )
        row = await cursor.fetchone()
        total = row["total"] or 0
        sent = row["sent"] or 0
        failed = row["failed"] or 0
        queued = row["queued"] or 0
        return {
            "total": total,
            "sent": sent,
            "failed": failed,
            "queued": queued,
            "success_rate": round(sent / total * 100, 1) if total > 0 else 0,
        }
    finally:
        await db.close()


def _row_to_dict(row) -> dict:
    return {
        "id": row["id"],
        "account_id": row["account_id"],
        "message_id": row["message_id"],
        "direction": row["direction"],
        "from_addr": row["from_addr"],
        "to_addr": row["to_addr"],
        "subject": row["subject"],
        "status": row["status"],
        "error": row["error"],
        "created_at": row["created_at"],
        "sent_at": row["sent_at"],
    }
