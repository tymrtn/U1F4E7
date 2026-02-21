# Envelope Email - Architecture

## System Overview

```
Agent / Client
    |
    v
REST API (FastAPI)
    |
    +---> Credential Store (encrypted, SQLite)
    |
    +---> SMTP Transport
    |         |
    |         +---> Connection Pool (per-account, semaphore-limited)
    |         +---> Send Worker (background queue, exponential backoff retry)
    |         +---> Direct Send (synchronous, wait=true)
    |
    +---> Mail Server Discovery
    |         |
    |         +---> DNS (SRV + MX records)
    |         +---> Autoconfig XML (Mozilla Thunderbird DB)
    |         +---> Provider alias expansion (Gmail, Outlook)
    |         +---> Port probing (465/587/993)
    |         +---> SSE streaming endpoint for real-time UI
    |
    +---> Message Tracking (SQLite)
    |         |
    |         +---> Status: queued → sending → sent/failed/retry
    |         +---> Stats dashboard (total, sent, failed, success rate)
    |
    +---> IMAP Read Transport
    |         |
    |         +---> Search (IMAP SEARCH, paginated)
    |         +---> Fetch (full RFC822 parse, attachments)
    |         +---> Folders (LIST command)
    |         +---> [Future] Track (IMAP folder polling)
    |
    +---> Drafts Primitive
    |         |
    |         +---> CRUD (SQLite-backed, status: draft → sent | discarded)
    |         +---> Send (draft → SMTP pipeline → message tracking)
    |         +---> Metadata (JSON field for agent context)
    |
    +---> [Future] Agent Primitives
              +---> Approval Gate (human review before send)
              +---> Reply Threading (In-Reply-To / References)
              +---> Audit Log (every action, every message)
```

## Components

### 1. API Layer (FastAPI)

The public interface. REST endpoints for all operations.

- `POST /send` - Send email (sync with `wait: true`, async with `wait: false`)
- `GET /messages` - List sent messages with status
- `GET /messages/{id}` - Get message details
- `GET /stats` - Send stats (total, sent, failed, success rate)
- `POST /accounts` - Register IMAP/SMTP credentials
- `GET /accounts` - List accounts
- `DELETE /accounts/{id}` - Remove account (invalidates connection pool)
- `POST /accounts/{id}/verify` - Test SMTP connection via pool
- `GET /accounts/discover?email=` - Auto-discover mail server settings
- `GET /accounts/discover/stream?email=` - SSE progressive discovery
- `POST /accounts/{id}/drafts` - Create draft
- `GET /accounts/{id}/drafts` - List drafts (limit/offset)
- `GET /accounts/{id}/drafts/{draft_id}` - Get draft
- `PUT /accounts/{id}/drafts/{draft_id}` - Update draft (409 if not draft status)
- `POST /accounts/{id}/drafts/{draft_id}/send` - Send draft via SMTP pipeline
- `DELETE /accounts/{id}/drafts/{draft_id}` - Discard draft
- `GET /accounts/{id}/inbox` - Paginated inbox (folder, limit, offset, q params)
- `GET /accounts/{id}/inbox/{uid}` - Full message with attachments
- `GET /accounts/{id}/folders` - List IMAP folders

### 2. Credential Store

Encrypted storage for IMAP/SMTP credentials. Each account entry holds:

- IMAP host, port, username, password/app-password
- SMTP host, port, username, password/app-password
- Display name, default signature
- Connection status and last-verified timestamp

MVP: SQLite with encrypted credential fields. Fernet symmetric encryption, key from environment variable.

### 3. SMTP Transport

**Connection Pool** (`transport/pool.py`): Per-account connection pooling with configurable limits. Features:
- Semaphore-limited concurrency (default 2 per account)
- NOOP validation before reuse
- Credential versioning — pool auto-invalidates when account credentials change
- Idle timeout and max lifetime eviction
- Background cleanup task

**Send Worker** (`transport/worker.py`): Background queue processor for async sends (`wait: false`). Features:
- Claims messages atomically (prevents double-send)
- Exponential backoff retry: 30s → 60s → 120s, capped at 600s
- Max 3 retries for connection errors
- Auth errors fail permanently (no retry)
- Orphan recovery on startup (resets `sending` → `queued`)
- In-flight tracking prevents duplicate processing

**Direct Send** (`transport/smtp.py`): Synchronous SMTP via `aiosmtplib`. Handles STARTTLS (587) and implicit TLS (465), MIME construction with text/HTML multipart.

### 4. Mail Server Discovery

Auto-discovers SMTP/IMAP settings from an email address. Three data sources queried concurrently:

1. **DNS SRV records** — `_submissions._tcp`, `_imaps._tcp`
2. **Autoconfig XML** — Mozilla Thunderbird database + domain-specific autoconfig
3. **MX records** — Extracts provider domain, expands aliases (e.g., google.com → gmail.com)

All candidates are port-probed in parallel. Best result by priority (SRV > autoconfig > MX > common patterns).

SSE streaming endpoint (`/accounts/discover/stream`) pushes phase updates to the dashboard in real time: dns → autoconfig → aliases → probing → complete.

### 5. Persistence

SQLite with WAL mode for concurrent reads. Tables:
- `accounts` — Credentials encrypted with Fernet (key from `ENVELOPE_SECRET_KEY`)
- `messages` — Status tracking with retry metadata (retry_count, next_retry_at)
- `drafts` — Agent compose primitive (status: draft → sent | discarded, JSON metadata)

### 6. IMAP Read Transport

Sync `imaplib` via `asyncio.to_thread()` for non-blocking IMAP access. Features:
- **Search**: IMAP SEARCH with pagination (reverse-sorted UIDs, sliced)
- **Fetch**: Full RFC822 parse with text/HTML body and attachment metadata
- **Folders**: LIST command for available mailboxes

Endpoints scoped to `/accounts/{id}/inbox` and `/accounts/{id}/folders`. IMAP auth/connection errors return 502.

### 7. Drafts Primitive

SQLite-backed compose primitive for agent workflows. Status machine: `draft` → `sent` | `discarded`.
- **Create/Update**: Compose with to, subject, text/html, in_reply_to, JSON metadata
- **Send**: Builds MIME, creates message tracking record, sends via SMTP pool
- **Discard**: Soft delete (status = discarded)

### 8. Future Components

- **IMAP IDLE Polling**: Real-time inbox monitoring via IDLE command
- **Approval Gates**: Human review before send
- **Webhook Emulation**: IMAP IDLE → HTTP callbacks for inbound events

## Design Principles

- **No vendor lock-in**: IMAP/SMTP is the protocol. Works with any mail server.
- **Agent-safe defaults**: Approval gates on by default. Nothing sends without human review unless explicitly configured otherwise.
- **Minimal infrastructure**: No message queues, no event buses, no external dependencies beyond the mail server and a SQLite file.
- **Async throughout**: All I/O is async. A single process handles many concurrent connections.
