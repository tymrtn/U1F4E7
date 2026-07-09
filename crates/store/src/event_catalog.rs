// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Canonical event-type catalog for the durable event-push pipeline.
//!
//! Every event that can flow to an [`crate::models::EventRoute`] has a stable
//! `event_type` string. These constants are the single source of truth so the
//! emit sites (watch, drafts, governor gate) and the route-matching layer agree
//! on the wire names. Serialized names are a public contract — never rename a
//! constant's value without a compatibility note.

/// A new message arrived in a watched folder.
pub const NEW_MESSAGE: &str = "new_message";
/// A one-time passcode was detected in a new message.
pub const OTP_DETECTED: &str = "otp_detected";
/// A draft transitioned into an approved / ready-to-send state.
pub const DRAFT_APPROVED: &str = "draft_approved";
/// A send was queued into the outbox (a `send_after` cooldown was set).
pub const SEND_QUEUED: &str = "send_queued";
/// A send completed successfully (the message left the outbox).
pub const SEND_COMPLETED: &str = "send_completed";
/// The Governor gate blocked an outbound send.
pub const GOVERNOR_BLOCKED: &str = "governor_blocked";
/// An agent-attributed action was recorded to the action log.
pub const AGENT_ACTION: &str = "agent_action";

/// All catalog event types, for validation and documentation.
pub const ALL_EVENT_TYPES: &[&str] = &[
    NEW_MESSAGE,
    OTP_DETECTED,
    DRAFT_APPROVED,
    SEND_QUEUED,
    SEND_COMPLETED,
    GOVERNOR_BLOCKED,
    AGENT_ACTION,
];

/// Is `event_type` a known catalog event? Unknown types are still deliverable
/// (routes may match on anything), but callers can use this to warn on typos.
pub fn is_known_event_type(event_type: &str) -> bool {
    ALL_EVENT_TYPES.contains(&event_type)
}

use crate::db::Database;
use crate::errors::Result;
use crate::models::Event;

impl Database {
    /// Emit a lifecycle catalog event (draft/send/governor/agent transitions).
    ///
    /// These events carry no mailbox contents — only the transition and an
    /// optional structured `payload` — so the caller is responsible for keeping
    /// secrets and full recipient addresses out of `payload`. The row is marked
    /// acked immediately (lifecycle events are informational, not an inbox
    /// action queue) and attributed to `agent_id` when present.
    pub fn emit_catalog_event(
        &self,
        account_id: &str,
        event_type: &str,
        payload: Option<serde_json::Value>,
        agent_id: Option<&str>,
    ) -> Result<Event> {
        let now = chrono::Utc::now().to_rfc3339();
        let event = Event {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: account_id.to_string(),
            event_type: event_type.to_string(),
            folder: "lifecycle".to_string(),
            uid: None,
            message_id: None,
            from_addr: None,
            subject: None,
            snippet: None,
            payload: payload.map(|p| p.to_string()),
            idempotency_key: None,
            secure_pending: false,
            acked_at: Some(now.clone()),
            created_at: now,
        };
        self.insert_event_with_agent(&event, agent_id)?;
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_event_types_round_trip() {
        assert!(is_known_event_type(SEND_QUEUED));
        assert!(is_known_event_type(GOVERNOR_BLOCKED));
        assert!(!is_known_event_type("totally_made_up"));
    }

    #[test]
    fn emit_catalog_event_persists_and_attributes() {
        let db = Database::open_memory().unwrap();
        let event = db
            .emit_catalog_event(
                "acc-1",
                SEND_QUEUED,
                Some(serde_json::json!({"draft_id": "d1", "cooldown_seconds": 30})),
                Some("agent-skippy"),
            )
            .unwrap();

        let stored = db.get_event(&event.id).unwrap().unwrap();
        assert_eq!(stored.event_type, SEND_QUEUED);
        assert_eq!(stored.folder, "lifecycle");
        assert!(stored.acked_at.is_some(), "lifecycle events are pre-acked");

        let agent: Option<String> = db
            .conn()
            .query_row(
                "SELECT agent_id FROM events WHERE id = ?1",
                [&event.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(agent.as_deref(), Some("agent-skippy"));
    }
}
