---
id: story-004
status: backlog
priority: medium
estimated_points: 3
depends_on: [story-001]
---

# Story-004: Add basic send tracking with SQLite persistence [API]

**As a** developer sending email through Envelope
**I want** to track whether my sent messages were delivered
**So that** my agents can detect failures and retry or escalate

## Context

After story-001 enables SMTP sending, we need to track what was sent and its status. This story adds a `sends` table to SQLite and a `GET /track/{id}` endpoint.

Delivery verification uses IMAP: check the Sent folder for the message (confirms the server accepted it). Bounce detection comes later as part of webhook emulation.

## Acceptance Criteria

- [ ] Every successful `POST /send` creates a record in the `sends` table
- [ ] Record includes: message ID, from, to, subject, timestamp, status
- [ ] `GET /track/{id}` returns the send record with current status
- [ ] `GET /sends` returns a paginated list of sent messages
- [ ] Status values: `queued`, `sent`, `failed`
- [ ] Failed sends include error message in the record
- [ ] SQLite persistence -- survives app restart

## Technical Notes

- Extends the SQLite database from story-003
- Schema: `sends` table with `id`, `account_id`, `message_id`, `from_addr`, `to_addr`, `subject`, `status`, `error`, `created_at`, `sent_at`
- IMAP-based delivery verification (checking Sent folder) is a stretch goal for this story -- basic status tracking is sufficient for MVP

## Regression Check

Run BEFORE starting (baseline) and AFTER completing (verify no breakage):

```bash
cd U1F4E7 && uvicorn app.main:app --host 0.0.0.0 --port 8000 &
sleep 2

# Existing endpoints
curl -s http://localhost:8000/ | head -5
curl -s http://localhost:8000/accounts | python3 -m json.tool

# Send and track
SEND_RESULT=$(curl -s -X POST http://localhost:8000/send \
  -H "Content-Type: application/json" \
  -d '{"from_email":"test@example.com","to":"dest@example.com","subject":"test","text":"hello"}')
echo $SEND_RESULT | python3 -m json.tool

# Track endpoint
curl -s http://localhost:8000/sends | python3 -m json.tool

kill %1
```

## Affected Files

**New:**
- `app/tracking.py` (send record CRUD)

**Modified:**
- `app/main.py` (add `/track/{id}` and `/sends` endpoints, record sends)
- `app/db.py` (add `sends` table migration)

**Reference:**
- `ARCHITECTURE.md` (component 3: Track)
