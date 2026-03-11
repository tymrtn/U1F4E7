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

## P0 Gap Resolutions

These decisions were identified by PM review and are now resolved for implementation.

### 1. State Machine

Draft lifecycle states (ordered):

```
draft → pending_review → sent (via approve)
                       → rejected (via reject, with optional feedback)
                       → expired (via timeout, no human action taken)

draft → blocked → sent (via manual override/approve)
                → rejected
                → expired

draft → sent (via auto-send, confidence ≥ threshold)
```

**States:** `draft`, `pending_review`, `blocked`, `sent`, `rejected`, `expired`, `escalated`

- `expired`: set automatically when `pending_review` or `blocked` draft exceeds its response window without action. Draft remains unsent.
- `escalated`: set when a reminder has been sent and the response window is close to expiring. Informational — draft is still actionable until `expired`.
- `rejected` and `expired` are terminal. No further action except recompose.
- `sent` is terminal. No undo.

### 2. Escalation Policy (Pilot Defaults)

| Timer | Action |
|-------|--------|
| 0 min | Draft created, notification sent via Telegram |
| 4 hours | Reminder sent ("Draft still pending: {subject}") |
| 24 hours | Status set to `expired`, final notification ("Draft expired: {subject}") |

- **No auto-send. Ever.** Expired means unsent.
- Timers are per-account configurable via `notification_config.reminder_after_minutes` (default 240) and `notification_config.expire_after_minutes` (default 1440).
- `blocked` drafts follow the same timers.
- Quiet hours do NOT pause expiration timers — they only suppress notifications. Morning catch-up digest includes anything that happened overnight.

### 3. Idempotency & Concurrency

- **Approve is idempotent.** If draft is already `sent`, return `{"status": "sent", "already": true}` with 200. Do not re-send.
- **Approve on expired/rejected draft** returns `409 Conflict` with current state and reason.
- **Reject is idempotent.** If draft is already `rejected`, return `{"status": "rejected", "already": true}` with 200.
- **Reject on sent draft** returns `409 Conflict` — can't unsend.
- **Double-tap protection:** Telegram callback handler checks draft state before calling the API. If stale, update the button message to show current state.
- **Pilot constraint: single reviewer (Tyler).** No multi-reviewer conflicts to handle. Multi-reviewer is a P1 for team expansion.

### 4. Auth / Session Semantics

- `POST /auth/review-token` returns a signed JWT.
- **Token is redeemed on first use.** Redemption creates a 30-minute scoped session cookie.
- **Token expiry:** 15 minutes from creation. After that, the URL returns a clear "link expired" page with a CTA to request a new link.
- **Session scope:** read access to the review queue + approve/reject actions for the specified account. NOT full admin.
- **Session expiry:** 30 minutes from redemption, sliding (resets on activity). After expiry, user sees re-auth prompt — no data loss (unsaved feedback text preserved client-side).
- **Token + session are independent.** A redeemed token cannot be re-used even if the session is still active.

### 5. Default Config (No `notification_config`)

- **Draft routing works normally** regardless of notification config. Missing config never blocks email operations.
- API compose response includes `"notification": {"status": "unconfigured"}` so the calling agent knows.
- Dashboard shows an account health warning: "Notifications not configured. Pending drafts will only be visible in the review queue."
- `start_here` tool output includes a setup prompt for notification preferences if unconfigured.
- **Pilot shortcut:** For the Tyler + Skippy pilot, notification config is irrelevant — Skippy sends Telegram messages directly via OpenClaw. The config becomes relevant when other users/agents use Envelope without OpenClaw.

### 6. Notification Contract (Webhooks)

When a draft transitions to `pending_review` or `blocked`:

**Webhook payload:**
```json
{
  "event_id": "uuid",
  "event_type": "draft.pending_review",
  "timestamp": "ISO-8601",
  "account_id": "uuid",
  "draft_id": "uuid",
  "to_addr": "recipient@example.com",
  "subject": "Email subject",
  "confidence": 0.72,
  "justification": "Why the agent composed this",
  "routing_status": "pending_review",
  "review_url": "https://envelope.../review?token=..."
}
```

**Delivery rules:**
- **Timeout:** 5 seconds per attempt
- **Retries:** 1 retry after 30 seconds on failure (timeout, 5xx, connection error)
- **No retry on:** 4xx responses (client error = permanent failure)
- **Logging:** Every webhook attempt logged in `action_log` with: URL, status code, latency, success/failure
- **Non-blocking:** Webhook delivery never delays the API response. Fires async after draft creation.
- **Circuit breaker:** After 5 consecutive failures to the same webhook URL, suppress further attempts for 1 hour and log a warning.

### PM Review: Additional Adjustments

- **`pending_review` moved from `urgent_events` to `batch_events` by default.** Routine approvals should not wake users at night. Only `escalation` and `blocked` are urgent by default.
- **Rejection feedback scope:** Recipient + thread scoped (not global). Capped to 5 most recent rejection feedbacks per recipient pattern.

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
