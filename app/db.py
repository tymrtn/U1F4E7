# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import os
import aiosqlite

DB_PATH = os.getenv("ENVELOPE_DB_PATH", "envelope.db")

SCHEMA = """
CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    smtp_host TEXT NOT NULL,
    smtp_port INTEGER NOT NULL DEFAULT 587,
    imap_host TEXT NOT NULL,
    imap_port INTEGER NOT NULL DEFAULT 993,
    username TEXT NOT NULL,
    encrypted_password TEXT NOT NULL,
    smtp_username TEXT,
    encrypted_smtp_password TEXT,
    imap_username TEXT,
    encrypted_imap_password TEXT,
    display_name TEXT,
    approval_required INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    verified_at TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    message_id TEXT,
    direction TEXT NOT NULL DEFAULT 'outbound',
    from_addr TEXT NOT NULL,
    to_addr TEXT NOT NULL,
    subject TEXT,
    status TEXT NOT NULL DEFAULT 'queued',
    error TEXT,
    created_at TEXT NOT NULL,
    sent_at TEXT,
    text_content TEXT,
    html_content TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    next_retry_at TEXT,
    FOREIGN KEY (account_id) REFERENCES accounts(id)
);

CREATE TABLE IF NOT EXISTS agent_actions (
    id TEXT PRIMARY KEY,
    inbound_message_id TEXT NOT NULL UNIQUE,
    from_addr TEXT,
    subject TEXT,
    classification TEXT,
    confidence REAL,
    action TEXT,
    reasoning TEXT,
    draft_reply TEXT,
    escalation_note TEXT,
    outbound_message_id TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS drafts (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'draft',
    to_addr TEXT NOT NULL,
    subject TEXT,
    text_content TEXT,
    html_content TEXT,
    in_reply_to TEXT,
    metadata TEXT,
    message_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    sent_at TEXT,
    created_by TEXT,
    FOREIGN KEY (account_id) REFERENCES accounts(id)
);

CREATE TABLE IF NOT EXISTS thread_links (
    message_id TEXT NOT NULL,
    references_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    discovered_at TEXT NOT NULL,
    PRIMARY KEY (message_id, references_id)
);

CREATE TABLE IF NOT EXISTS message_embeddings (
    message_id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    embedding BLOB NOT NULL,
    model TEXT NOT NULL,
    embedded_at TEXT NOT NULL
);
"""

MIGRATIONS = [
    "ALTER TABLE messages ADD COLUMN text_content TEXT",
    "ALTER TABLE messages ADD COLUMN html_content TEXT",
    "ALTER TABLE messages ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE messages ADD COLUMN next_retry_at TEXT",
    "ALTER TABLE accounts ADD COLUMN auto_send_threshold REAL NOT NULL DEFAULT 0.85",
    "ALTER TABLE accounts ADD COLUMN review_threshold REAL NOT NULL DEFAULT 0.50",
    "ALTER TABLE drafts ADD COLUMN send_after TEXT",
    "ALTER TABLE drafts ADD COLUMN snoozed_until TEXT",
    "ALTER TABLE accounts ADD COLUMN rate_limit_per_hour INTEGER",
    # Story 011 — Policy tables
    'CREATE TABLE IF NOT EXISTS domain_policies (account_id TEXT PRIMARY KEY, name TEXT, description TEXT, "values" TEXT, tone TEXT, style TEXT, kb_text TEXT, updated_at TEXT)',
    "CREATE TABLE IF NOT EXISTS address_policies (account_id TEXT NOT NULL, pattern TEXT NOT NULL, purpose TEXT, reply_instructions TEXT, escalation_rules TEXT, routing_rules TEXT, trash_criteria TEXT, help_resources TEXT, sensitive_topics TEXT, confidence_threshold REAL DEFAULT 0.7, webhook_url TEXT, updated_at TEXT, PRIMARY KEY (account_id, pattern))",
    # Story 013 — Action log
    "CREATE TABLE IF NOT EXISTS action_log (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, action_type TEXT NOT NULL, confidence REAL NOT NULL, justification TEXT NOT NULL, action_taken TEXT NOT NULL, message_id TEXT, draft_id TEXT, created_at TEXT NOT NULL)",
    # Story 015 — Webhook fields
    "ALTER TABLE accounts ADD COLUMN webhook_url TEXT",
    "ALTER TABLE accounts ADD COLUMN webhook_secret TEXT",
    "CREATE TABLE IF NOT EXISTS webhook_state (account_id TEXT PRIMARY KEY, last_uid TEXT, updated_at TEXT)",
]

_connection: aiosqlite.Connection | None = None


async def get_db() -> aiosqlite.Connection:
    global _connection
    if _connection is None:
        _connection = await aiosqlite.connect(DB_PATH)
        _connection.row_factory = aiosqlite.Row
        await _connection.execute("PRAGMA journal_mode=WAL")
        await _connection.execute("PRAGMA busy_timeout=5000")
        await _connection.execute("PRAGMA foreign_keys=ON")
    return _connection


async def close_db():
    global _connection
    if _connection is not None:
        await _connection.close()
        _connection = None


async def init_db():
    db = await get_db()
    await db.executescript(SCHEMA)
    # Run migrations for existing databases (ALTER TABLE is idempotent-safe)
    for migration in MIGRATIONS:
        try:
            await db.execute(migration)
        except Exception:
            pass  # Column already exists
    await db.commit()
