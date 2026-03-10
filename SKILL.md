---
name: envelope
description: Send, read, and manage email through the Envelope API (u1f4e7.com). Use when composing email, reading inbox, managing drafts, or handling approval workflows. Envelope enforces blind routing — always provide confidence scores and justifications. NOT for: sending via raw SMTP/IMAP directly.
---

# Envelope Email API

Envelope is a REST email API with approval gates and blind routing. You interact via HTTP, not MCP.

## Auth

All requests require `Authorization: Bearer <token>` header. The token and base URL are configured per deployment.

## Core Workflow

```
1. compose_email → POST /accounts/{id}/drafts (with confidence + justification)
2. System routes blind:
   - confidence ≥ 0.85 → auto-sent
   - 0.50–0.84 → pending_review (human approves/rejects)
   - < 0.50 → blocked
3. You see the outcome, you don't control routing
```

## API Quick Reference

### Compose Email (blind routed)
```
POST /accounts/{account_id}/drafts
{
  "to": "recipient@example.com",
  "subject": "Subject line",
  "text_content": "Email body",
  "confidence": 0.92,
  "justification": "Routine reply to known contact, matches policy",
  "created_by": "agent"
}
→ {"id": "...", "status": "sent"|"pending_review"|"blocked", ...}
```

### List Inbox
```
GET /accounts/{account_id}/inbox?limit=10
→ [{"uid": "22", "message_id": "...", "from_addr": "Name <email>", "subject": "...", "date": "...", "flags": [], "size": 12133}]
```
Note: listing returns metadata only, no body. Use the UID to fetch full message.

### Read Message (by UID)
```
GET /accounts/{account_id}/inbox/{uid}
→ {"uid": "22", "from_addr": "...", "to_addr": "...", "subject": "...", "text_body": "...", "html_body": "...", "attachments": [...]}
```
⚠️ Field names: inbox uses `text_body`/`html_body`. Sent messages DB (`/messages/{id}`) uses `text_content`/`html_content`.

### Read Sent Message (from DB)
```
GET /messages/{message_id}
→ {"from_addr": "...", "to_addr": "...", "text_content": "...", "html_content": "...", ...}
```
This is for messages Envelope sent, stored in the local DB. Not for reading inbox.

### List Drafts
```
GET /accounts/{account_id}/drafts
→ [{"id": "...", "to": "...", "subject": "...", "status": "pending_review", ...}]
```

### Approve Draft
```
POST /accounts/{account_id}/drafts/{draft_id}/approve
→ Draft is sent
```

### Reject Draft
```
POST /accounts/{account_id}/drafts/{draft_id}/reject
{"reason": "Wrong recipient"}
→ Draft rejected with feedback
```

### Search
```
GET /accounts/{account_id}/search?q=keyword&limit=5
→ [{"message_id": "...", "subject": "...", ...}]
```

## Confidence Scoring Guidelines

Score based on policy match, not your desire to send:

| Confidence | When to use |
|---|---|
| 0.90–1.0 | Known recipient, routine content, matches all policies |
| 0.70–0.89 | Known recipient, slightly novel content or tone |
| 0.50–0.69 | New recipient, sensitive topic, or uncertain policy match |
| 0.0–0.49 | Unknown recipient, confidential content, or policy conflict |

**Always provide justification.** Cite the specific policy or context that informed your score.

## Rules

- Never fabricate email addresses — verify from inbox or user instruction
- Always include `created_by: "agent"` on drafts
- If approval_required is true on account, all drafts need human approval regardless of confidence
- CC rules: from @aposema.com → CC tyler@aposema.com; from other → CC ty@tmrtn.com
- When in doubt about recipients or content, use low confidence and let the system route to review
