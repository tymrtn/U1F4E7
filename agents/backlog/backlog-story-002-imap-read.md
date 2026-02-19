---
id: story-002
status: backlog
priority: high
estimated_points: 5
depends_on: [story-003]
---

# Story-002: Implement IMAP inbox read via aioimaplib [API]

**As a** developer building an agent that processes inbound email
**I want** a `GET /inbox` endpoint that reads messages from my mailbox
**So that** my agents can monitor and respond to incoming email

## Context

Envelope needs bidirectional email access. Story-001 covers outbound (SMTP send). This story covers inbound (IMAP read). Together they form the core transport layer.

Uses `aioimaplib` for async IMAP connections. The API should support listing messages, fetching individual messages, and basic search.

## Acceptance Criteria

- [ ] `GET /inbox` returns a paginated list of messages from the configured IMAP account
- [ ] `GET /message/{id}` returns a single message with headers, body (text and HTML), and attachment metadata
- [ ] Supports `limit` and `offset` query parameters for pagination
- [ ] Supports `folder` query parameter (default: INBOX)
- [ ] Supports basic search via `q` query parameter (IMAP SEARCH command)
- [ ] Parses MIME messages correctly -- handles multipart, inline images, attachments
- [ ] Async -- does not block the event loop during IMAP operations
- [ ] Returns structured error on authentication failure or connection timeout

## Technical Notes

- Use `aioimaplib` (already in requirements.txt)
- MIME parsing via `email` stdlib (`email.message_from_bytes`, `email.policy`)
- Consider a `transport/imap.py` module to isolate IMAP logic
- IMAP connections should be pooled or reused where possible (connection setup is expensive)
- Message IDs: use IMAP UID for stable references

## Regression Check

Run BEFORE starting (baseline) and AFTER completing (verify no breakage):

```bash
cd U1F4E7 && uvicorn app.main:app --host 0.0.0.0 --port 8000 &
sleep 2

# Existing endpoints still work
curl -s http://localhost:8000/ | head -5
curl -s -X POST http://localhost:8000/send \
  -H "Content-Type: application/json" \
  -d '{"from_email":"test@example.com","to":"dest@example.com","subject":"test","text":"hello"}' | python3 -m json.tool

# New endpoints respond
curl -s http://localhost:8000/inbox | python3 -m json.tool
curl -s http://localhost:8000/message/1 | python3 -m json.tool

kill %1
```

## Affected Files

**New:**
- `app/transport/imap.py`

**Modified:**
- `app/main.py` (add `/inbox` and `/message/{id}` endpoints)

**Reference:**
- `ARCHITECTURE.md` (component 3: IMAP/SMTP Transport)
