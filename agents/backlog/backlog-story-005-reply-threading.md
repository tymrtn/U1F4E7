---
id: story-005
status: backlog
priority: medium
estimated_points: 5
depends_on: [story-001, story-002]
---

# Story-005: Add reply-to threading for agent conversations [API]

**As a** developer building an agent that carries on email conversations
**I want** replies sent through Envelope to thread correctly in the recipient's inbox
**So that** agent-sent replies appear in the same conversation as the original message

## Context

This is the first agent primitive. When an agent reads an inbound message (story-002) and sends a reply (story-001), the reply must include correct `In-Reply-To` and `References` headers so email clients thread the conversation properly.

Without this, every agent reply appears as a new conversation -- confusing for recipients and breaking context.

## Acceptance Criteria

- [ ] `POST /send` accepts an optional `in_reply_to` parameter (message ID of the message being replied to)
- [ ] When `in_reply_to` is provided, Envelope sets `In-Reply-To` and `References` headers on the outbound message
- [ ] `References` header includes the full chain (not just the immediate parent)
- [ ] Subject line automatically prefixed with `Re:` if not already present
- [ ] Thread context retrievable: `GET /thread/{message_id}` returns all messages in a conversation thread
- [ ] Threading works across multiple reply levels (A -> B -> C all in same thread)

## Technical Notes

- `In-Reply-To`: contains the Message-ID of the parent message
- `References`: contains the Message-IDs of all ancestors in the thread, space-separated
- Thread reconstruction: query IMAP for messages with matching `References` or `In-Reply-To` headers
- Consider storing thread relationships in SQLite for faster lookups
- RFC 2822 / RFC 5322 define the header semantics

## Regression Check

Run BEFORE starting (baseline) and AFTER completing (verify no breakage):

```bash
cd U1F4E7 && uvicorn app.main:app --host 0.0.0.0 --port 8000 &
sleep 2

# Existing endpoints
curl -s http://localhost:8000/ | head -5
curl -s http://localhost:8000/accounts | python3 -m json.tool
curl -s http://localhost:8000/inbox | python3 -m json.tool

# Send with reply threading
curl -s -X POST http://localhost:8000/send \
  -H "Content-Type: application/json" \
  -d '{"from_email":"test@example.com","to":"dest@example.com","subject":"Re: test","text":"reply","in_reply_to":"<original-msg-id@example.com>"}' | python3 -m json.tool

# Thread lookup
curl -s http://localhost:8000/thread/test-id | python3 -m json.tool

kill %1
```

## Affected Files

**New:**
- `app/primitives/__init__.py`
- `app/primitives/threading.py`

**Modified:**
- `app/main.py` (add `in_reply_to` to SendEmail model, add `/thread/{id}` endpoint)
- `app/transport/smtp.py` (set In-Reply-To and References headers)
- `app/db.py` (add `threads` table)

**Reference:**
- `ARCHITECTURE.md` (component 4: Reply Threading)
