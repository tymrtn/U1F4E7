// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Real-time dashboard event bus (SSE fan-out).
//!
//! A single [`tokio::sync::broadcast`] channel carries typed [`DashboardEvent`]s
//! from the places that already know something changed — the unsnooze sweep, the
//! scheduled-send sweep, and the draft/compose mutation handlers — to any live
//! `GET /api/events/stream` subscriber. The channel lives in
//! [`crate::state::AppState`]; publishing is a fire-and-forget
//! [`EventBus::publish`] with no back-pressure on the producer.
//!
//! ## Privacy: account-metadata level only
//! Events are deliberately *thin*. They carry ids, counts, and status strings —
//! never message bodies, subjects, recipient addresses, tokens, or secrets. The
//! SSE channel is safe to expose at the same trust boundary as the authenticated
//! dashboard, but it must never become a body/subject side-channel. The Governor
//! outcome summary on [`send_status`](DashboardEvent::SendStatus) is a decision +
//! optional block code, matching what the sanitized `send_governor.*` audit
//! event already records.
//!
//! ## Bounded channel and lagged receivers
//! Capacity is [`EVENT_CHANNEL_CAPACITY`] (256). `broadcast` keeps the most
//! recent N events; a slow subscriber that falls behind by more than N receives
//! [`tokio::sync::broadcast::error::RecvError::Lagged`] with the count of dropped
//! events, then resumes from the oldest retained event. The SSE handler treats a
//! lag as a non-fatal hint — it emits a `type: "lagged"` control frame so the
//! browser client can resync via a full poll — and keeps the stream open. A
//! publish when there are zero subscribers is a no-op (returns `Err`, ignored).

use serde::Serialize;
use tokio::sync::broadcast;

/// Broadcast channel capacity. The channel retains at most this many recent
/// events; a subscriber lagging by more than this is signalled `Lagged` and
/// resumes from the oldest retained event rather than blocking any publisher.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// A real-time dashboard event, serialized to the SSE `data:` payload as
/// `{ "type": <kind>, "account_id"?: <id>, "payload": { … } }`.
///
/// The `type` string is the SSE `event:` field *and* is duplicated into the JSON
/// body so a client using `onmessage` (rather than per-type listeners) can still
/// discriminate. Payloads are metadata only — no bodies, subjects, recipients,
/// or secrets.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DashboardEvent {
    /// New mail was indexed for an account during a unified-inbox refresh.
    /// Carries post-refresh counts only, never message content.
    NewMail {
        account_id: String,
        /// Total messages indexed for the account's inbox after the refresh.
        message_count: usize,
        /// Unread count after the refresh.
        unread_count: usize,
    },
    /// A draft was created/queued (compose, reply, or an approved queue-to-outbox).
    DraftQueued {
        account_id: String,
        draft_id: String,
        /// Coarse origin: `"compose"`, `"reply"`, or `"queue"`.
        origin: &'static str,
    },
    /// A draft changed status via an operator action (approve/discard/block/edit).
    DraftStatusChanged {
        account_id: String,
        draft_id: String,
        /// New status string, e.g. `"draft"`, `"blocked"`, `"discarded"`.
        status: String,
    },
    /// A scheduled draft reached the send sweep. Carries the delivery outcome and
    /// a sanitized Governor summary — decision + optional block code, no
    /// recipients or bodies.
    SendStatus {
        account_id: String,
        draft_id: String,
        /// `"sent"`, `"sent_unrecorded"` (SMTP accepted but local sent-state
        /// persistence failed — not durable success), `"blocked"`,
        /// `"deferred"`, or `"failed"`.
        outcome: &'static str,
        /// Governor decision (`"allow"`/`"review"`/`"deny"`/…) when the gate ran.
        governor_decision: Option<String>,
        /// Governor block code (e.g. `"governor_blocked"`) when not allowed.
        governor_block_code: Option<String>,
    },
    /// A snoozed message was moved back to its origin folder by the sweep or the
    /// unsnooze action.
    Unsnoozed {
        account_id: String,
        /// The origin folder the message was returned to (folder name only).
        original_folder: String,
    },
    /// An account's derived health changed (last-known-state badge).
    AccountHealth {
        account_id: String,
        /// `"healthy"`, `"unhealthy"`, or `"unknown"`.
        status: &'static str,
    },
}

impl DashboardEvent {
    /// The SSE `event:` field / JSON `type` discriminant for this event.
    pub fn kind(&self) -> &'static str {
        match self {
            DashboardEvent::NewMail { .. } => "new_mail",
            DashboardEvent::DraftQueued { .. } => "draft_queued",
            DashboardEvent::DraftStatusChanged { .. } => "draft_status_changed",
            DashboardEvent::SendStatus { .. } => "send_status",
            DashboardEvent::Unsnoozed { .. } => "unsnoozed",
            DashboardEvent::AccountHealth { .. } => "account_health",
        }
    }
}

/// Cloneable handle to the dashboard broadcast channel. Held in `AppState`.
///
/// Publishing never blocks a producer: it fans out to current subscribers and
/// drops on the floor when there are none. Subscribers are created lazily by the
/// SSE handler via [`EventBus::subscribe`].
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<DashboardEvent>,
}

impl EventBus {
    /// Create a bus with the default [`EVENT_CHANNEL_CAPACITY`].
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Subscribe to future events. The returned receiver only sees events
    /// published *after* it is created.
    pub fn subscribe(&self) -> broadcast::Receiver<DashboardEvent> {
        self.tx.subscribe()
    }

    /// Publish an event to all current subscribers. Returns the number of
    /// receivers it reached; `0` (no subscribers) is normal and ignored by
    /// callers. Never blocks or errors in a way a producer must handle.
    pub fn publish(&self, event: DashboardEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Current subscriber count (for tests / diagnostics).
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_matches_serialized_type_tag() {
        let ev = DashboardEvent::NewMail {
            account_id: "acc1".into(),
            message_count: 3,
            unread_count: 1,
        };
        assert_eq!(ev.kind(), "new_mail");
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "new_mail");
        assert_eq!(json["account_id"], "acc1");
        assert_eq!(json["message_count"], 3);
        assert_eq!(json["unread_count"], 1);
    }

    #[test]
    fn send_status_summary_carries_no_recipients_or_bodies() {
        let ev = DashboardEvent::SendStatus {
            account_id: "acc1".into(),
            draft_id: "d1".into(),
            outcome: "blocked",
            governor_decision: Some("review".into()),
            governor_block_code: Some("governor_blocked".into()),
        };
        assert_eq!(ev.kind(), "send_status");
        let json = serde_json::to_string(&ev).unwrap();
        // Metadata only — the serialized frame must not contain fields we never set.
        assert!(!json.contains("recipient"));
        assert!(!json.contains("body"));
        assert!(!json.contains("subject"));
        assert!(json.contains("governor_blocked"));
    }

    #[tokio::test]
    async fn publish_reaches_live_subscriber() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let reached = bus.publish(DashboardEvent::AccountHealth {
            account_id: "acc1".into(),
            status: "unhealthy",
        });
        assert_eq!(reached, 1);
        let got = rx.recv().await.unwrap();
        assert_eq!(got.kind(), "account_health");
    }

    #[test]
    fn publish_with_no_subscribers_is_a_noop() {
        let bus = EventBus::new();
        assert_eq!(
            bus.publish(DashboardEvent::Unsnoozed {
                account_id: "acc1".into(),
                original_folder: "INBOX".into(),
            }),
            0
        );
    }

    #[tokio::test]
    async fn slow_subscriber_lags_then_resumes_from_oldest_retained() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        // Overflow the channel by more than its capacity so the subscriber lags.
        for i in 0..(EVENT_CHANNEL_CAPACITY + 10) {
            bus.publish(DashboardEvent::DraftQueued {
                account_id: "acc1".into(),
                draft_id: format!("d{i}"),
                origin: "compose",
            });
        }
        // The first recv reports the lag with the dropped count, then the stream
        // resumes from the oldest retained event rather than closing.
        let err = rx.recv().await.unwrap_err();
        match err {
            broadcast::error::RecvError::Lagged(n) => assert!(n >= 10),
            other => panic!("expected Lagged, got {other:?}"),
        }
        // Subsequent recv succeeds — the receiver is not poisoned by a lag.
        assert!(rx.recv().await.is_ok());
    }
}
