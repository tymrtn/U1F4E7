# Story 021 — Notification Engine & Human Approval Flow

**Status:** Spec  
**Priority:** P0 — Blocks shipping  
**Depends on:** Blind routing (987381d ✅), approval/reject endpoints (✅), notification_email schema (0f66f21 ✅)

---

## Problem

Blind routing works. The agent composes, the server routes based on confidence, and the approve/reject endpoints exist. But when a draft lands in `pending_review`, **nobody knows**. The human owner has zero visibility into what the agent is doing unless they manually check the dashboard.

Sending another email to notify the human about an email is not the answer. The notification surface must be where the human already lives.

---

## Design: Pareto Version (Tyler + Skippy on Telegram)

### Primary Surface: Telegram via OpenClaw

When a draft hits `pending_review` or `blocked`, Skippy sends a Telegram message with:

```
📨 Draft awaiting approval

To: nate@travellemming.com
Subject: Re: Paper collaboration
Confidence: 0.72
Reason: New topic not covered by address policy

"Hey Nate, thanks for connecting us with Prof. Tang..."

[Approve] [Reject] [Review ↗]
```

**Three inline buttons:**
- **Approve** → `POST /accounts/{id}/drafts/{draft_id}/approve` → sends immediately, confirms in chat
- **Reject** → Prompts "What was wrong?" in Telegram → captures feedback → stores on draft metadata + action log
- **Review ↗** → Deep link to dashboard: `https://envelope.../dashboard?token={jwt}&draft={draft_id}`

### Deep Link Authentication

- `POST /auth/review-token` generates a short-lived JWT (15 min expiry)
- Token is scoped: grants access to specific draft OR review queue, not full admin
- Single-use or time-limited (whichever comes first)
- Token payload: `{ account_id, draft_id?, scope: "review"|"queue", exp, iat }`
- Dashboard checks token on load, establishes session cookie for the visit

### Rejection Feedback Loop

When the human rejects a draft:

1. Skippy asks "What was wrong?" in Telegram
2. Human replies with text (e.g., "too formal", "wrong recipient", "don't reply to this person")
3. Feedback stored on:
   - `draft.metadata.rejection_feedback` — what was wrong with THIS draft
   - `draft.metadata.rejected_at` — timestamp
   - `draft.metadata.rejected_by` — "human" or identifier
4. Feedback logged in `action_log` with type `draft_reject`
5. Next time the agent composes for this recipient/thread, the policy context includes prior rejection feedback

### Inbound Activity Digest

Hourly (configurable), Skippy sends a batch summary:

```
📬 Envelope Activity (12:00-13:00)

Read: 5 emails
├ Auto-replied (0.91): Invoice receipt from Stripe
├ Auto-replied (0.88): Calendar confirmation from Google
├ Drafted for review (0.63): Partnership inquiry from unknown@startup.io
├ Escalated: Legal notice from law-firm@example.com
└ No action: Newsletter from morning-brew

Pending: 2 drafts awaiting approval
```

Each item includes a deep link to the specific message or draft in the dashboard.

**Digest rules:**
- Only send if there's activity (no empty digests)
- Urgent events (escalation, blocked draft) break the batch — sent immediately
- Configurable interval: 30min / 1hr / 4hr / daily
- Quiet hours: suppress digest during configured sleep hours, deliver morning summary

### Dashboard Views

The dashboard (already exists at `/dashboard`) needs three sections:

#### 1. Review Queue (exists, needs enhancement)
- Filter by status: pending_review, blocked, rejected
- Each draft shows: recipient, subject, body preview, confidence, justification, thread context
- Approve/Reject buttons with feedback field
- Deep-linkable by draft_id

#### 2. Activity Feed (new)
- Chronological log of all agent actions
- Filterable by: action_type, confidence range, account, date
- Each entry shows: timestamp, action type, confidence, justification, outcome
- Link to related draft/message

#### 3. Policy Editor (new)
- Domain policy form: name, description, values, tone, style, kb_text
- Address policy list: pattern, purpose, reply_instructions, escalation_rules, confidence_threshold
- Add/edit/delete address policies
- Deep-linkable by pattern (so rejection flow can link directly to "update policy for nate@...")

---

## API Changes

### New Endpoints

```
POST /auth/review-token
  Body: { account_id, draft_id?, scope: "review"|"queue" }
  Returns: { token, expires_at, url }
  Auth: API key required
```

### Modified Endpoints

```
POST /accounts/{id}/drafts/{draft_id}/reject
  Body: { reason, feedback? }  ← add optional feedback field
  Stores feedback in draft metadata + action_log
```

### Account Config Extension

Add to account model (migration):

```sql
ALTER TABLE accounts ADD COLUMN notification_config TEXT;
-- JSON blob:
-- {
--   "telegram_chat_id": "6493121275",
--   "digest_interval_minutes": 60,
--   "quiet_hours_start": "22:00",
--   "quiet_hours_end": "08:00",
--   "quiet_hours_tz": "Europe/Madrid",
--   "urgent_events": ["escalation", "blocked", "pending_review"],
--   "batch_events": ["auto_sent", "inbound_read", "no_action"]
-- }
```

---

## Implementation Order

### Phase A: Notification wiring (1 day)
1. When `route_composed_email()` creates a `pending_review` or `blocked` draft:
   - Fire webhook to `account.webhook_url` if configured (fire-and-forget)
   - Return notification metadata in the response so the calling agent/system can act on it
2. `POST /auth/review-token` endpoint for deep links

### Phase B: Telegram integration via OpenClaw (1 day)
3. Skippy sends Telegram message with draft summary + inline buttons (Approve/Reject/Review)
4. Approve button → hits approve endpoint → confirms in chat
5. Reject button → prompts for feedback → hits reject endpoint with feedback
6. Review button → deep link with review token

### Phase C: Dashboard enhancements (2-3 days)
7. Review queue: accept token auth, show specific draft when deep-linked
8. Activity feed: chronological action log with filters
9. Policy editor: forms for domain + address policies, linked from rejection flow
10. Rejection feedback field on reject action

### Phase D: Digest (1 day)
11. Background task aggregates activity per interval
12. Sends formatted summary via webhook/notification channel
13. Quiet hours suppression + morning catch-up

---

## What This Is NOT

- Not a generic notification SaaS — this is Envelope's approval UX
- Not building a Telegram bot — using OpenClaw's existing channel infrastructure
- Not replacing the API — dashboard is a convenience layer, API remains primary
- Not building mobile apps — Telegram IS the mobile app
- No email notifications for email events — that's insane

---

## Exit Criteria

A human using Envelope through OpenClaw can:
1. Receive a Telegram notification when an agent drafts an email that needs approval
2. Approve or reject the draft from Telegram with one tap
3. Provide rejection feedback that improves future agent behavior
4. Click through to the dashboard to see the full email and edit policies
5. Receive an hourly digest of all agent email activity
6. Access the dashboard via a deep link without logging in separately
