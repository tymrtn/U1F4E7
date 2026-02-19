---
id: story-001
status: backlog
priority: high
estimated_points: 5
depends_on: [story-003]
blocks: [story-004]
---

# Story-001: Implement SMTP send transport via aiosmtplib [API]

**As a** developer integrating Envelope into an agent workflow
**I want** the `/send` endpoint to deliver email through my SMTP server
**So that** my agents can send email from my own mailbox without a third-party sending service

## Context

The `/send` endpoint currently returns a stub response. This story replaces the stub with real SMTP delivery using `aiosmtplib`. The caller provides message content; Envelope connects to the configured SMTP server and sends.

Depends on story-003 (credential management) for retrieving stored SMTP credentials. Can be developed in parallel using hardcoded test credentials, but full integration requires the credential store.

## Acceptance Criteria

- [ ] `POST /send` connects to the account's SMTP server and delivers the message
- [ ] Supports STARTTLS and direct TLS connections
- [ ] Constructs proper MIME message with text and/or HTML parts
- [ ] Returns message ID from the SMTP server on success
- [ ] Returns structured error on authentication failure, connection timeout, or rejected recipient
- [ ] Async -- does not block the event loop during SMTP operations
- [ ] Works with Gmail (app password), Outlook, Migadu, and Fastmail SMTP settings

## Technical Notes

- Use `aiosmtplib` (already in requirements.txt)
- MIME construction via `email.mime` stdlib
- SMTP config comes from credential store (story-003) or request payload for MVP
- Consider a `transport/smtp.py` module to isolate SMTP logic from the API layer
- Set proper `Message-ID`, `Date`, `From`, `To`, `Subject` headers

## Regression Check

Run BEFORE starting (baseline) and AFTER completing (verify no breakage):

```bash
# App starts without error
cd U1F4E7 && uvicorn app.main:app --host 0.0.0.0 --port 8000 &
sleep 2

# Health check -- dashboard loads
curl -s http://localhost:8000/ | head -5

# API docs accessible
curl -s http://localhost:8000/docs

# Send endpoint responds (currently stub)
curl -s -X POST http://localhost:8000/send \
  -H "Content-Type: application/json" \
  -d '{"from_email":"test@example.com","to":"dest@example.com","subject":"test","text":"hello"}' | python3 -m json.tool

kill %1
```

## Affected Files

**New:**
- `app/transport/__init__.py`
- `app/transport/smtp.py`

**Modified:**
- `app/main.py` (replace stub with SMTP transport call)

**Reference:**
- `ARCHITECTURE.md` (component 3: IMAP/SMTP Transport)
