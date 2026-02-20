# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import uuid
from datetime import datetime, timezone
from typing import Optional

from app.credentials.crypto import encrypt, decrypt
from app.db import get_db


async def create_account(
    name: str,
    smtp_host: str,
    smtp_port: int,
    imap_host: str,
    imap_port: int,
    username: str,
    password: str,
    smtp_username: Optional[str] = None,
    smtp_password: Optional[str] = None,
    imap_username: Optional[str] = None,
    imap_password: Optional[str] = None,
    display_name: Optional[str] = None,
    approval_required: bool = True,
) -> dict:
    account_id = str(uuid.uuid4())
    now = datetime.now(timezone.utc).isoformat()

    encrypted_password = encrypt(password)
    encrypted_smtp_pw = encrypt(smtp_password) if smtp_password else None
    encrypted_imap_pw = encrypt(imap_password) if imap_password else None

    db = await get_db()
    await db.execute(
        """INSERT INTO accounts
        (id, name, smtp_host, smtp_port, imap_host, imap_port,
         username, encrypted_password, smtp_username, encrypted_smtp_password,
         imap_username, encrypted_imap_password, display_name,
         approval_required, created_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
        (
            account_id, name, smtp_host, smtp_port, imap_host, imap_port,
            username, encrypted_password, smtp_username, encrypted_smtp_pw,
            imap_username, encrypted_imap_pw, display_name,
            1 if approval_required else 0, now,
        ),
    )
    await db.commit()

    return {
        "id": account_id,
        "name": name,
        "smtp_host": smtp_host,
        "smtp_port": smtp_port,
        "imap_host": imap_host,
        "imap_port": imap_port,
        "username": username,
        "display_name": display_name,
        "approval_required": approval_required,
        "created_at": now,
    }


async def list_accounts() -> list[dict]:
    db = await get_db()
    cursor = await db.execute(
        """SELECT id, name, smtp_host, smtp_port, imap_host, imap_port,
                  username, smtp_username, imap_username, display_name,
                  approval_required, created_at, verified_at
           FROM accounts ORDER BY created_at DESC"""
    )
    rows = await cursor.fetchall()
    return [_row_to_dict(row) for row in rows]


async def get_account(account_id: str) -> Optional[dict]:
    db = await get_db()
    cursor = await db.execute(
        """SELECT id, name, smtp_host, smtp_port, imap_host, imap_port,
                  username, smtp_username, imap_username, display_name,
                  approval_required, created_at, verified_at
           FROM accounts WHERE id = ?""",
        (account_id,),
    )
    row = await cursor.fetchone()
    return _row_to_dict(row) if row else None


async def get_account_with_credentials(account_id: str) -> Optional[dict]:
    """Returns account with decrypted credentials. Internal use only."""
    db = await get_db()
    cursor = await db.execute("SELECT * FROM accounts WHERE id = ?", (account_id,))
    row = await cursor.fetchone()
    if not row:
        return None

    result = _row_to_dict(row)
    result["password"] = decrypt(row["encrypted_password"])

    # Resolve effective SMTP/IMAP credentials
    result["effective_smtp_username"] = row["smtp_username"] or row["username"]
    result["effective_smtp_password"] = (
        decrypt(row["encrypted_smtp_password"])
        if row["encrypted_smtp_password"]
        else result["password"]
    )
    result["effective_imap_username"] = row["imap_username"] or row["username"]
    result["effective_imap_password"] = (
        decrypt(row["encrypted_imap_password"])
        if row["encrypted_imap_password"]
        else result["password"]
    )
    return result


async def delete_account(account_id: str) -> bool:
    db = await get_db()
    cursor = await db.execute("DELETE FROM accounts WHERE id = ?", (account_id,))
    await db.commit()
    return cursor.rowcount > 0


async def update_verified(account_id: str):
    db = await get_db()
    now = datetime.now(timezone.utc).isoformat()
    await db.execute(
        "UPDATE accounts SET verified_at = ? WHERE id = ?",
        (now, account_id),
    )
    await db.commit()


def _row_to_dict(row) -> dict:
    return {
        "id": row["id"],
        "name": row["name"],
        "smtp_host": row["smtp_host"],
        "smtp_port": row["smtp_port"],
        "imap_host": row["imap_host"],
        "imap_port": row["imap_port"],
        "username": row["username"],
        "smtp_username": row["smtp_username"],
        "imap_username": row["imap_username"],
        "display_name": row["display_name"],
        "approval_required": bool(row["approval_required"]),
        "created_at": row["created_at"],
        "verified_at": row["verified_at"],
    }
