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
    +---> IMAP/SMTP Transport
    |         |
    |         +---> Send (SMTP via aiosmtplib)
    |         +---> Read (IMAP via aioimaplib)
    |         +---> Track (IMAP folder polling)
    |
    +---> Agent Primitives
    |         |
    |         +---> Draft Preview (compose + hold)
    |         +---> Approval Gate (human review before send)
    |         +---> Reply Threading (In-Reply-To / References)
    |         +---> Signature Manager (per-account templates)
    |         +---> Audit Log (every action, every message)
    |
    +---> Webhook Emulation
              |
              +---> Delivery status via IMAP polling
              +---> Inbound message notifications
```

## Components

### 1. API Layer (FastAPI)

The public interface. REST endpoints for all operations.

- `POST /send` - Send email (or create draft for approval)
- `GET /inbox` - Read messages from connected account
- `GET /message/{id}` - Get specific message with full thread
- `POST /draft` - Create draft for review
- `POST /approve/{id}` - Approve a held draft for sending
- `GET /track/{id}` - Delivery and read status
- `POST /accounts` - Register IMAP/SMTP credentials
- `GET /audit` - Query the audit log

### 2. Credential Store

Encrypted storage for IMAP/SMTP credentials. Each account entry holds:

- IMAP host, port, username, password/app-password
- SMTP host, port, username, password/app-password
- Display name, default signature
- Connection status and last-verified timestamp

MVP: SQLite with encrypted credential fields. Fernet symmetric encryption, key from environment variable.

### 3. IMAP/SMTP Transport

The core. Direct connections to the user's mail server.

**Send (SMTP)**: `aiosmtplib` for async SMTP. Handles STARTTLS, authentication, MIME construction, attachments.

**Read (IMAP)**: `aioimaplib` for async IMAP. Folder listing, message fetch, search, flag management.

**Track**: IMAP-based delivery tracking. Check Sent folder for message status. Poll for bounces and replies.

### 4. Agent Primitives

The differentiator. Features that make email safe and useful for autonomous agents.

**Draft Preview**: Agent composes a message that enters a held state. Human can review, edit, approve, or reject via API or webhook callback. Nothing sends without explicit approval (when gates are enabled).

**Approval Gate**: Configurable per-account or per-request. When enabled, `POST /send` creates a draft instead of sending. The draft must be explicitly approved via `POST /approve/{id}`.

**Reply Threading**: Automatic `In-Reply-To` and `References` header management. When replying to a message, Envelope maintains the thread so replies appear correctly in the recipient's client.

**Signature Manager**: Per-account signature templates. Agents don't need to know or manage signature blocks -- Envelope appends the correct signature based on the sending account.

**Audit Log**: Append-only log of every API action. Who requested what, when, what happened. Queryable via `GET /audit`.

### 5. Webhook Emulation

Since IMAP/SMTP has no native push mechanism (outside of IMAP IDLE), Envelope emulates webhooks:

- Periodic IMAP polling for new messages and status changes
- HTTP callback to registered webhook URLs when events occur
- Event types: `message.received`, `message.sent`, `message.bounced`, `message.replied`

### 6. Persistence

**MVP**: SQLite for everything -- accounts, drafts, audit log, message cache.

**Later**: Postgres when concurrent access or query complexity demands it. The schema stays the same; only the driver changes.

## Design Principles

- **No vendor lock-in**: IMAP/SMTP is the protocol. Works with any mail server.
- **Agent-safe defaults**: Approval gates on by default. Nothing sends without human review unless explicitly configured otherwise.
- **Minimal infrastructure**: No message queues, no event buses, no external dependencies beyond the mail server and a SQLite file.
- **Async throughout**: All I/O is async. A single process handles many concurrent connections.
