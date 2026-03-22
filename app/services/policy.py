# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import json
from datetime import datetime, timezone

from app.db import get_db


async def upsert_domain_policy(account_id: str, **fields) -> dict:
    db = await get_db()
    now = datetime.now(timezone.utc).isoformat()
    values_json = json.dumps(fields["values"]) if fields.get("values") is not None else None
    await db.execute(
        """INSERT OR REPLACE INTO domain_policies
           (account_id, name, description, "values", tone, style, kb_text, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
        (
            account_id,
            fields.get("name"),
            fields.get("description"),
            values_json,
            fields.get("tone"),
            fields.get("style"),
            fields.get("kb_text"),
            now,
        ),
    )
    await db.commit()
    return await get_domain_policy(account_id)


async def get_domain_policy(account_id: str) -> dict | None:
    db = await get_db()
    cursor = await db.execute(
        "SELECT * FROM domain_policies WHERE account_id = ?",
        (account_id,),
    )
    row = await cursor.fetchone()
    return _domain_policy_to_dict(row) if row else None


async def upsert_address_policy(account_id: str, pattern: str, **fields) -> dict:
    db = await get_db()
    now = datetime.now(timezone.utc).isoformat()
    sensitive_json = (
        json.dumps(fields["sensitive_topics"])
        if fields.get("sensitive_topics") is not None
        else None
    )
    await db.execute(
        """INSERT OR REPLACE INTO address_policies
           (account_id, pattern, purpose, reply_instructions, escalation_rules,
            routing_rules, trash_criteria, help_resources, sensitive_topics,
            confidence_threshold, webhook_url, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
        (
            account_id,
            pattern,
            fields.get("purpose"),
            fields.get("reply_instructions"),
            fields.get("escalation_rules"),
            fields.get("routing_rules"),
            fields.get("trash_criteria"),
            fields.get("help_resources"),
            sensitive_json,
            fields.get("confidence_threshold", 0.7),
            fields.get("webhook_url"),
            now,
        ),
    )
    await db.commit()
    return await get_address_policy(account_id, pattern)


async def list_address_policies(account_id: str) -> list[dict]:
    db = await get_db()
    cursor = await db.execute(
        "SELECT * FROM address_policies WHERE account_id = ? ORDER BY pattern",
        (account_id,),
    )
    rows = await cursor.fetchall()
    return [_address_policy_to_dict(row) for row in rows]


async def get_address_policy(account_id: str, pattern: str) -> dict | None:
    db = await get_db()
    cursor = await db.execute(
        "SELECT * FROM address_policies WHERE account_id = ? AND pattern = ?",
        (account_id, pattern),
    )
    row = await cursor.fetchone()
    return _address_policy_to_dict(row) if row else None


async def delete_address_policy(account_id: str, pattern: str) -> bool:
    db = await get_db()
    cursor = await db.execute(
        "DELETE FROM address_policies WHERE account_id = ? AND pattern = ?",
        (account_id, pattern),
    )
    await db.commit()
    return cursor.rowcount > 0


def _domain_policy_to_dict(row) -> dict:
    raw_values = row["values"]
    values = None
    if raw_values:
        try:
            values = json.loads(raw_values)
        except Exception:
            values = raw_values
    return {
        "account_id": row["account_id"],
        "name": row["name"],
        "description": row["description"],
        "values": values,
        "tone": row["tone"],
        "style": row["style"],
        "kb_text": row["kb_text"],
        "updated_at": row["updated_at"],
    }


def _address_policy_to_dict(row) -> dict:
    def _parse(v):
        if v is None:
            return None
        try:
            return json.loads(v)
        except Exception:
            return v

    return {
        "account_id": row["account_id"],
        "pattern": row["pattern"],
        "purpose": row["purpose"],
        "reply_instructions": row["reply_instructions"],
        "escalation_rules": row["escalation_rules"],
        "routing_rules": row["routing_rules"],
        "trash_criteria": row["trash_criteria"],
        "help_resources": row["help_resources"],
        "sensitive_topics": _parse(row["sensitive_topics"]),
        "confidence_threshold": row["confidence_threshold"] if row["confidence_threshold"] is not None else 0.7,
        "webhook_url": row["webhook_url"],
        "updated_at": row["updated_at"],
    }
