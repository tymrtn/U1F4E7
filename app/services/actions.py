# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import uuid
from datetime import datetime, timezone

from app.db import get_db


async def log_action(
    account_id: str,
    action_type: str,
    confidence: float,
    justification: str,
    action_taken: str,
    message_id: str | None = None,
    draft_id: str | None = None,
) -> dict:
    db = await get_db()
    log_id = str(uuid.uuid4())
    now = datetime.now(timezone.utc).isoformat()
    await db.execute(
        """INSERT INTO action_log
           (id, account_id, action_type, confidence, justification, action_taken,
            message_id, draft_id, created_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)""",
        (log_id, account_id, action_type, confidence, justification,
         action_taken, message_id, draft_id, now),
    )
    await db.commit()
    return {
        "id": log_id,
        "account_id": account_id,
        "action_type": action_type,
        "confidence": confidence,
        "justification": justification,
        "action_taken": action_taken,
        "message_id": message_id,
        "draft_id": draft_id,
        "created_at": now,
    }


async def list_actions(
    account_id: str,
    limit: int = 50,
    offset: int = 0,
    draft_id: str | None = None,
    message_id: str | None = None,
) -> list[dict]:
    db = await get_db()
    conditions = ["account_id = ?"]
    params: list = [account_id]
    if draft_id:
        conditions.append("draft_id = ?")
        params.append(draft_id)
    if message_id:
        conditions.append("message_id = ?")
        params.append(message_id)
    where = " AND ".join(conditions)
    params += [limit, offset]
    cursor = await db.execute(
        f"SELECT * FROM action_log WHERE {where} ORDER BY created_at DESC LIMIT ? OFFSET ?",
        params,
    )
    rows = await cursor.fetchall()
    return [_row_to_dict(row) for row in rows]


async def get_action(log_id: str) -> dict | None:
    db = await get_db()
    cursor = await db.execute(
        "SELECT * FROM action_log WHERE id = ?",
        (log_id,),
    )
    row = await cursor.fetchone()
    return _row_to_dict(row) if row else None


def _row_to_dict(row) -> dict:
    return {
        "id": row["id"],
        "account_id": row["account_id"],
        "action_type": row["action_type"],
        "confidence": row["confidence"],
        "justification": row["justification"],
        "action_taken": row["action_taken"],
        "message_id": row["message_id"],
        "draft_id": row["draft_id"],
        "created_at": row["created_at"],
    }
