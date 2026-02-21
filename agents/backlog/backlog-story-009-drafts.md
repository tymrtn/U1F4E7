---
id: story-009
status: active
priority: high
estimated_points: 5
depends_on: [story-001, story-003]
---

# Story-009: Draft Compose Primitive [API]

**As a** developer building an agent that composes email
**I want** CRUD endpoints for draft messages with a send action
**So that** agents can compose safely with human review before sending

## Context

Agents need a compose-then-review workflow. A draft is created, optionally edited, then either sent or discarded. This is the first agent primitive -- it enables human-in-the-loop email composition.

Drafts are stored in SQLite (not IMAP drafts folder) for speed and simplicity. The send action reuses the existing SMTP pipeline (pool + worker).

## Acceptance Criteria

- [ ] `POST /accounts/{id}/drafts` creates a draft with to, subject, text/html, optional in_reply_to
- [ ] `GET /accounts/{id}/drafts` lists drafts with limit/offset pagination
- [ ] `GET /accounts/{id}/drafts/{draft_id}` returns a single draft
- [ ] `PUT /accounts/{id}/drafts/{draft_id}` updates a draft (only if status=draft, 409 otherwise)
- [ ] `POST /accounts/{id}/drafts/{draft_id}/send` sends draft via SMTP pipeline and marks it sent
- [ ] `DELETE /accounts/{id}/drafts/{draft_id}` marks draft as discarded
- [ ] Draft status flow: `draft` -> `sent` | `discarded`
- [ ] Send creates a message tracking record (reuses messages module)
- [ ] In-Reply-To header set on sent message when draft has in_reply_to field
- [ ] Metadata field stored as JSON for agent-specific context

## Technical Notes

- New `drafts` table in SQLite schema
- New `app/drafts.py` module following `app/messages.py` pattern
- Send flow: get draft -> build MIME -> create message record -> send via pool -> mark sent
- Reuse `build_mime_message` from `transport/smtp.py`

## Regression Check

```bash
cd U1F4E7 && python -m pytest tests/ -v
```

## Affected Files

**New:**
- `app/drafts.py`
- `tests/test_drafts.py`

**Modified:**
- `app/db.py` (add drafts table)
- `app/main.py` (add draft endpoints)
