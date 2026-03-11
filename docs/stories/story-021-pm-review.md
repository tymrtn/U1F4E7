# Story 021 PM Review — Notification Engine & Human Approval Flow

## Summary

This story is strong for a Tyler + Skippy pilot, but it is not yet fully specified for a reliable product release. The core UX is right: notify the human where they already are, make approve/reject fast, preserve a feedback loop, and keep the dashboard as the source of truth.

The main issue is not feature scope. It is missing product decisions around state, concurrency, reliability, and authorization. Those gaps will surface immediately in production because this workflow sits on the boundary between "draft" and "email already sent". The spec should be tightened before implementation starts beyond a narrow pilot.

My recommendation:

- Approve Phase A/B for a single-reviewer pilot only after the state model, idempotency rules, and escalation behavior are defined.
- Do not treat Phase C/D as straightforward UI work. They depend on the same unresolved product contracts.

## 1. Edge Cases

| Scenario | Expected behavior | PM review |
|---|---|---|
| Multiple drafts hit `pending_review` simultaneously | Each draft becomes its own queue item and its own actionable notification. Actions are always draft-scoped, never "latest draft" scoped. Queue order should be deterministic: urgent first, then oldest first. | Pilot can tolerate one Telegram message per draft. What cannot be ambiguous is dedupe and ordering. We need a unique notification/event ID per draft transition so retries do not create duplicate approval actions. |
| Human does not respond within the escalation window | The draft must never auto-send by default. It should move to an explicit terminal or semi-terminal state such as `expired` or `escalated`, remain unsent, and create a follow-up notification/reminder path. | The story references escalation but does not define the timeout, reminder cadence, fallback reviewer, or final state. This is a P0 gap. "Still pending forever" is not acceptable because it hides unresolved work. |
| Token expires while human is mid-review in dashboard | The deep-link token should only be used to enter the review surface. Once redeemed, the dashboard should create a scoped review session cookie so the user is not interrupted mid-review. If the session expires before action, Approve/Reject returns a clear auth error and prompts the user to reopen the link. | "Single-use or time-limited (whichever comes first)" is ambiguous. We need one precise rule. Recommended: single-use on redemption, then 30-minute idle review session. |
| Agent recomposes after rejection | Yes, it should reference the feedback. The next compose request for the same recipient/thread should include the prior rejection feedback and the model should cite that feedback in its justification. | The story says feedback is included next time, which is correct. Missing detail: scope. I recommend recipient/thread scoped by default, not global account-wide memory, and capped to the most recent relevant feedback items. |
| Network failure during approve (draft sent but confirmation lost) | Approve must be idempotent. A retry must return the already-final state and must never send the email twice. Telegram confirmation should reconcile against draft/message state if the callback response is lost. | This is the highest operational risk in the story. Today the product boundary is not explicit enough. The approve action needs an idempotency key or draft-version guard plus a read-after-write state check. |
| Quiet hours overlap with urgent escalation | True escalations should bypass quiet hours. Normal digests should not. `pending_review` should not automatically bypass quiet hours unless the account explicitly marks it urgent. | The sample config currently puts `pending_review` inside `urgent_events`. That will wake users up for routine approvals and erode trust quickly. Split "approval needed" from "urgent escalation". |
| Account has no `notification_config` set | Draft routing should still succeed, but the system should mark notification delivery as `unconfigured`, keep the item visible in the review queue, and surface an account-health warning in the dashboard/API response. | This is a required degraded mode. The story currently assumes configuration exists. For pilot usability, first-time setup should strongly encourage config before approval-required routing is enabled. |
| Webhook endpoint is down or slow | Draft creation must not block on notification delivery. Delivery should timeout quickly, be retried asynchronously, and log failure state for operators. | "Fire-and-forget" is directionally right but too vague. We need explicit timeout, retry, and observability behavior. Otherwise notification failures become silent data loss. |
| Human approves a draft that was already auto-expired | Approve should return a stale-state conflict (`409` or equivalent), not send anything, and show the latest draft state with a CTA to recompose or reopen the queue. | This requires a real state machine. Reusing `discarded` for rejection and expiration will make the UX and reporting muddy. Expiration should be explicit. |

### Recommended state additions

The story should define the lifecycle explicitly. At minimum:

- `pending_review`
- `blocked`
- `sent`
- `rejected`
- `expired`

If engineering wants to keep storage simpler, `rejected` and `expired` can still map to a shared low-level archive state, but the product/API contract should expose them distinctly.

## 2. User Personas

The Tyler + Skippy version is a good wedge, but this workflow has at least four materially different persona shapes.

| Persona | What they need | What changes from current story |
|---|---|---|
| Solo founder with AI email assistant | Fast mobile approvals, almost zero setup, digest + quiet hours that feel human, lightweight policy tuning. | Current story fits this persona best. We should optimize defaults, setup flow, and "approve in 1 tap" above everything else. Telegram can be the primary surface here. |
| Small team with shared inbox | Multiple reviewers, ownership clarity, collision handling, shared audit trail, possibly role-based approval rules. | Story is underspecified. We need assignment, "claimed by", who approved, and stale-action handling when two teammates act on the same draft. A single Telegram chat ID is not enough. |
| Enterprise compliance officer | Approval evidence, separation of duties, RBAC, retention, exportable audit logs, explicit escalation rules, often non-Telegram channels. | Telegram-only framing becomes a blocker. This persona needs policy-enforced approvals, stronger auth than URL JWTs alone, and a durable action/audit model. |
| Developer integrating Envelope into their product | Stable APIs, signed webhooks, idempotency, embeddable review links/UI, channel-agnostic notifications. | The story is too OpenClaw-specific for this persona. We should preserve a generic event contract and treat Telegram as one adapter, not the product contract itself. |

### Persona takeaways

- Solo founder is the launch persona.
- Small team is the first expansion persona and introduces concurrency requirements immediately.
- Enterprise compliance is a later persona, but its requirements expose weak spots in auth, audit, and state modeling now.
- Developer integrations require the notification system to be channel-agnostic even if Telegram is the first channel shipped.

## 3. User Journeys

### A. First-time setup: zero to first approved draft

1. User adds an email account and verifies it.
2. User enables approval-based routing or keeps the default thresholds that can produce `pending_review` / `blocked`.
3. User configures notifications:
   - connect Telegram/OpenClaw
   - choose digest interval
   - set quiet hours and timezone
   - define which events are urgent
4. System sends a test notification and a test review link.
5. Agent composes the first uncertain email.
6. Envelope routes it to `pending_review`.
7. User receives a Telegram notification with summary, reason, confidence, and actions.
8. User taps `Approve` or opens `Review`.
9. Envelope sends the draft, updates metadata, logs the action, and confirms back in Telegram.
10. User can view the completed action in the dashboard activity feed.

Critical onboarding requirement: the system must make it obvious when approval-required drafts can be created before notification delivery is configured.

### B. Daily usage: morning catch-up, mid-day approval, policy tuning

1. Quiet hours end and the user receives the first non-empty morning digest.
2. Digest summarizes activity since the prior delivery window and highlights drafts still waiting.
3. During the day, a new `pending_review` draft arrives and generates an immediate notification.
4. User approves directly from Telegram for low-context items.
5. For ambiguous items, user taps `Review`, reads thread context in the dashboard, and approves or rejects there.
6. User notices repeated approvals/rejections for the same sender pattern.
7. User opens the policy editor from the activity feed or rejection flow.
8. User adjusts address/domain policy so future routing and tone improve.
9. Over time, the daily review load should decrease because policy + rejection feedback improve routing quality.

### C. Rejection + policy update loop

1. Draft arrives in `pending_review`.
2. User rejects it from Telegram or the dashboard.
3. System prompts for feedback or lets the user submit without feedback.
4. Feedback is stored on the draft, tied to the rejecting actor, and written to the action log.
5. Dashboard offers a direct link to the relevant policy record for the sender/recipient pattern.
6. User updates address policy, domain policy, or both.
7. On the next compose for the same recipient/thread, Envelope injects the prior rejection feedback plus the updated policy into the prompt context.
8. The next draft justification explicitly references that learned guidance.
9. User sees either fewer rejections or a higher confidence score for similar emails.

Key product requirement: if the agent claims it learned from feedback, the justification should show what it used. Otherwise the feedback loop is opaque.

### D. Escalation handling

1. A draft is routed to `blocked`, or a policy marks the draft as urgent.
2. Envelope sends an immediate urgent notification to the configured channel.
3. If the user does not act within the configured reminder window, Envelope sends a reminder or escalates to a fallback reviewer/channel.
4. If the item is still unresolved at the escalation deadline, Envelope marks it `expired` or `escalated`, keeps it unsent, and records the event in the activity log.
5. Any later stale approval attempt returns a conflict and shows the latest state.
6. The morning digest includes unresolved or expired items if they were suppressed overnight.

This journey is missing the most detail in the current spec and should be fully defined before shipping.

## 4. Acceptance Criteria

These ACs should be attached to the story before engineering starts. They are written to be testable.

### Phase A — Notification wiring

- When `route_composed_email()` creates a draft with status `pending_review` or `blocked`, the API response includes the `draft_id`, routed `status`, and a `notification` object describing whether notification delivery was attempted, skipped, or unconfigured.
- If `account.webhook_url` is configured, Envelope emits a notification event containing at minimum: `event_id`, `event_type`, `account_id`, `draft_id`, `status`, `confidence`, `justification`, and `created_at`.
- Webhook delivery failure or timeout does not change the routed draft status and does not make the draft creation request fail.
- Notification delivery failures are recorded in a durable log or action stream visible to operators.
- `POST /auth/review-token` requires valid API auth and rejects unknown `account_id`, unknown `draft_id`, or invalid `scope`.
- A review token scoped to a single draft cannot be used to access another draft or another account.
- Expired or already-redeemed review tokens are rejected with a clear auth error.
- The story defines the degraded-mode behavior when no `notification_config` exists.

### Phase B — Telegram integration via OpenClaw

- A `pending_review` or `blocked` draft produces a Telegram message containing recipient, subject, confidence, routing reason, preview text, and three actions: `Approve`, `Reject`, and `Review`.
- `Approve` sends the draft exactly once even if Telegram retries the callback or the user taps twice.
- `Reject` captures optional freeform feedback and stores it on draft metadata with actor + timestamp.
- After a successful reject, the action is visible in the dashboard/activity log and the draft is no longer actionable.
- `Review` opens a deep link that lands the user in the scoped review surface for that draft.
- If the draft is already `sent`, `rejected`, or `expired` when a Telegram action arrives, the user receives a stale-state message and the action is not re-applied.
- Telegram confirmations reflect the final server state rather than assuming the callback succeeded.

### Phase C — Dashboard enhancements

- A valid review token allows the user to open the review surface without manually entering the global API key.
- Redeeming a review token establishes a scoped review session so the user can approve/reject multiple actions during the session without reauth on every click.
- If the review session expires before action, the UI shows a recoverable auth error and does not lose any unsaved feedback text.
- The review queue supports filters for all actionable and terminal review states defined by the story.
- A deep link to a specific `draft_id` loads that draft in focus and highlights it in the queue.
- Each review item shows recipient, subject, body preview, confidence, justification, and thread context.
- The activity feed supports filters by action type, confidence range, account, and date.
- Reject actions in the dashboard support optional feedback entry inline.
- The policy editor supports add/edit/delete for domain and address policies and supports deep-linking from a rejection flow.
- Concurrent actions are handled safely: if one reviewer resolves a draft, other open clients receive a clear stale-state error on subsequent actions.

### Phase D — Digest

- Envelope sends no digest when there has been no activity in the interval.
- Digest interval is configurable to `30m`, `1h`, `4h`, or `daily`.
- Non-urgent digest delivery is suppressed during configured quiet hours.
- Suppressed digest items appear in the first post-quiet-hours catch-up digest.
- Urgent events bypass quiet hours only when the event type is explicitly configured as urgent.
- Digest content includes item counts plus deep links to the related draft/message where applicable.
- Pending unresolved drafts are summarized in the digest separately from completed activity.
- A slow or unavailable notification endpoint/channel does not block digest generation; the failure is retried/logged.
- The same activity event is not included twice in the same digest window.

## 5. Risks & Mitigations

| Risk | Why it matters | Mitigation |
|---|---|---|
| Duplicate sends on approve retry | Worst possible failure: user loses trust immediately. | Make approve idempotent and state-aware. Use event IDs or draft version checks and reconcile confirmation against server state. |
| Notification fatigue | If every routine review is "urgent", users mute the channel and the system stops functioning. | Default `pending_review` to non-urgent, keep true escalations urgent, and batch where reasonable. |
| Silent notification failure | Draft exists but no human sees it. | Surface notification delivery state in API + dashboard, retry async, and alert on repeated failures. |
| Ambiguous draft states | Reporting, queue filters, and user trust break if `rejected`, `discarded`, and `expired` collapse into one bucket. | Define the product state machine now, even if storage remains simplified under the hood. |
| Weak auth model for deep links | Review links in Telegram are easy to forward or leak. | Keep tokens short-lived, single-use on redemption, scope tightly, and convert to a short session rather than long-lived URL auth. |
| Multi-reviewer race conditions | Shared inbox use cases will create conflicting approvals immediately. | Add stale-state detection, actor attribution, and eventual assignment/claiming rules. |
| Feedback loop becomes opaque or noisy | Freeform rejection text can be inconsistent and hard for the agent to use safely. | Scope feedback to recipient/thread, display what feedback was applied, and cap memory to recent relevant items. |
| Telegram/OpenClaw dependency becomes product bottleneck | Good pilot choice, weak long-term contract. | Treat notification delivery as a channel abstraction. Keep the product contract at webhook/event/API layer. |
| Quiet hours undermine urgent handling or vice versa | Either users get spammed or truly urgent items get delayed. | Separate digest quiet hours from urgent escalation policy and define default severity rules. |

## 6. Gaps

### P0 gaps to resolve before implementation

- Explicit state machine: the story needs product states for `rejected`, `expired`, and `escalated`, not just `pending_review` and `blocked`.
- Escalation policy: no timeout values, reminder behavior, fallback destination, or terminal outcome are defined.
- Idempotency/concurrency: approve/reject behavior under retries, duplicate callbacks, and multi-reviewer conflicts is not defined.
- Auth/session semantics: "single-use or time-limited" is not precise enough for implementation.
- Default configuration behavior: missing `notification_config` behavior is not specified.
- Notification contract: payload shape, timeout, retry policy, and delivery logging are not defined.

### P1 gaps that will matter quickly after pilot

- Multi-reviewer model: who can approve, how actions are attributed, and whether drafts can be claimed/assigned.
- Channel strategy: story is framed around Telegram/OpenClaw, but the product needs a generic notification abstraction.
- Policy UX details: the policy editor is named, but validation, change history, and impact preview are not defined.
- Observability: no success metrics, backlog metrics, SLA metrics, or notification failure metrics are specified.
- Onboarding UX: no setup wizard, test notification step, or account-health surface is defined.

### Gaps relative to the current implementation

- The story says the dashboard exists at `/dashboard`, but the current app serves the main dashboard at `/` and review queue at `/review`.
- The story wants a `rejected` review state, but the current reject flow discards drafts after adding feedback metadata.
- The story says rejection is logged to `action_log`, but the current reject endpoint stores metadata and discards the draft without logging `draft_reject`.
- The story introduces `notification_config`, while the current account model only includes `notification_email` plus `webhook_url`.

## Bottom Line

This story is directionally correct and should ship as a pilot, but only with explicit constraints:

- single primary reviewer
- Telegram/OpenClaw as the first channel, not the product contract
- no default overnight pings for routine `pending_review`
- idempotent approve/reject actions
- explicit expired/escalated handling

Without those decisions, engineering can build the happy path, but production behavior will be inconsistent exactly where trust matters most.
