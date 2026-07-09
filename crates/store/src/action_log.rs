// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::ActionLog;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

impl Database {
    /// Log an agent action and return the created record.
    pub fn log_action(
        &self,
        account_id: &str,
        action_type: &str,
        confidence: f64,
        justification: &str,
        action_taken: &str,
        message_id: Option<&str>,
        draft_id: Option<&str>,
    ) -> Result<ActionLog> {
        self.insert_action_log(ActionLogInsert {
            account_id,
            action_type,
            confidence,
            justification,
            action_taken,
            message_id,
            draft_id,
            event_id: None,
            action_status: "completed",
            agent_id: None,
        })
    }

    /// Log an action attributed to a specific agent. `agent_id` is `None` for
    /// human/legacy actions (stored as NULL); `Some` records the acting agent.
    #[allow(clippy::too_many_arguments)]
    pub fn log_action_with_agent(
        &self,
        account_id: &str,
        action_type: &str,
        confidence: f64,
        justification: &str,
        action_taken: &str,
        message_id: Option<&str>,
        draft_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<ActionLog> {
        self.insert_action_log(ActionLogInsert {
            account_id,
            action_type,
            confidence,
            justification,
            action_taken,
            message_id,
            draft_id,
            event_id: None,
            action_status: "completed",
            agent_id,
        })
    }

    /// Log an action linked to an event, returning the existing record on duplicate.
    pub fn log_action_for_event(&self, input: EventActionLogInput<'_>) -> Result<ActionLog> {
        self.log_action_for_event_with_agent(input, None)
    }

    /// Log an event-linked action attributed to a specific agent. `agent_id` is
    /// `None` for human/legacy actions (stored as NULL).
    pub fn log_action_for_event_with_agent(
        &self,
        input: EventActionLogInput<'_>,
        agent_id: Option<&str>,
    ) -> Result<ActionLog> {
        self.insert_action_log(ActionLogInsert {
            account_id: input.account_id,
            action_type: input.action_type,
            confidence: input.confidence,
            justification: input.justification,
            action_taken: input.action_taken,
            message_id: input.message_id,
            draft_id: input.draft_id,
            event_id: Some(input.event_id),
            action_status: input.action_status,
            agent_id,
        })
    }

    /// List recent actions for an account, newest first.
    pub fn list_actions(&self, account_id: &str, limit: u32) -> Result<Vec<ActionLog>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, action_type, confidence, justification, action_taken,
                    message_id, draft_id, event_id, action_status, created_at
             FROM action_log
             WHERE account_id = ?1
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;

        let actions = stmt
            .query_map(params![account_id, limit], map_action_log)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(actions)
    }

    /// List recent actions for an account attributed to a specific `agent_id`,
    /// newest first. Additive read (P1a writer freeze is over): mirrors
    /// [`list_actions`] with an extra `agent_id` predicate on the same shape.
    pub fn list_actions_for_agent(
        &self,
        account_id: &str,
        agent_id: &str,
        limit: u32,
    ) -> Result<Vec<ActionLog>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, action_type, confidence, justification, action_taken,
                    message_id, draft_id, event_id, action_status, created_at
             FROM action_log
             WHERE account_id = ?1 AND agent_id = ?2
             ORDER BY created_at DESC
             LIMIT ?3",
        )?;

        let actions = stmt
            .query_map(params![account_id, agent_id, limit], map_action_log)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(actions)
    }

    fn insert_action_log(&self, input: ActionLogInsert<'_>) -> Result<ActionLog> {
        let id = Uuid::new_v4().to_string();

        self.conn().execute(
            "INSERT OR IGNORE INTO action_log (
                id, account_id, action_type, confidence, justification, action_taken,
                message_id, draft_id, event_id, action_status, agent_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                input.account_id,
                input.action_type,
                input.confidence,
                input.justification,
                input.action_taken,
                input.message_id,
                input.draft_id,
                input.event_id,
                input.action_status,
                input.agent_id,
            ],
        )?;

        let existing_for_event = match input.event_id {
            Some(event_id) => self.get_action_by_event(event_id, input.action_type)?,
            None => None,
        };
        if let Some(existing) = existing_for_event {
            return Ok(existing);
        }

        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, action_type, confidence, justification, action_taken,
                    message_id, draft_id, event_id, action_status, created_at
             FROM action_log
             WHERE id = ?1",
        )?;
        Ok(stmt.query_row(params![id], map_action_log)?)
    }

    fn get_action_by_event(&self, event_id: &str, action_type: &str) -> Result<Option<ActionLog>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, action_type, confidence, justification, action_taken,
                    message_id, draft_id, event_id, action_status, created_at
             FROM action_log
             WHERE event_id = ?1 AND action_type = ?2",
        )?;
        Ok(stmt
            .query_row(params![event_id, action_type], map_action_log)
            .optional()?)
    }
}

pub struct EventActionLogInput<'a> {
    pub account_id: &'a str,
    pub event_id: &'a str,
    pub action_type: &'a str,
    pub confidence: f64,
    pub justification: &'a str,
    pub action_taken: &'a str,
    pub action_status: &'a str,
    pub message_id: Option<&'a str>,
    pub draft_id: Option<&'a str>,
}

struct ActionLogInsert<'a> {
    account_id: &'a str,
    action_type: &'a str,
    confidence: f64,
    justification: &'a str,
    action_taken: &'a str,
    message_id: Option<&'a str>,
    draft_id: Option<&'a str>,
    event_id: Option<&'a str>,
    action_status: &'a str,
    agent_id: Option<&'a str>,
}

fn map_action_log(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActionLog> {
    Ok(ActionLog {
        id: row.get(0)?,
        account_id: row.get(1)?,
        action_type: row.get(2)?,
        confidence: row.get(3)?,
        justification: row.get(4)?,
        action_taken: row.get(5)?,
        message_id: row.get(6)?,
        draft_id: row.get(7)?,
        event_id: row.get(8)?,
        action_status: row.get(9)?,
        created_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::db::Database;

    #[test]
    fn log_and_list_actions() {
        let db = Database::open_memory().unwrap();
        let account_id = "acc-test-1";

        let action = db
            .log_action(
                account_id,
                "auto_reply",
                0.85,
                "Sender is a known contact",
                "drafted reply",
                Some("<msg-123@example.com>"),
                Some("draft-abc"),
            )
            .unwrap();

        assert_eq!(action.account_id, account_id);
        assert_eq!(action.action_type, "auto_reply");
        assert!((action.confidence - 0.85).abs() < f64::EPSILON);
        assert_eq!(action.message_id.as_deref(), Some("<msg-123@example.com>"));
        assert_eq!(action.draft_id.as_deref(), Some("draft-abc"));
        assert_eq!(action.action_status, "completed");

        db.log_action(
            account_id,
            "classify",
            0.92,
            "High spam score",
            "marked as spam",
            None,
            None,
        )
        .unwrap();

        let actions = db.list_actions(account_id, 10).unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].action_type, "classify");
        assert_eq!(actions[1].action_type, "auto_reply");
    }

    #[test]
    fn agent_id_lands_and_legacy_writers_insert_null() {
        let db = Database::open_memory().unwrap();

        // Legacy writer: agent_id must be NULL.
        let legacy = db
            .log_action("acc-1", "classify", 0.5, "j", "t", None, None)
            .unwrap();
        let legacy_agent: Option<String> = db
            .conn()
            .query_row(
                "SELECT agent_id FROM action_log WHERE id = ?1",
                super::params![legacy.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_agent, None);

        // Agent-attributed writer stores the id.
        let attributed = db
            .log_action_with_agent(
                "acc-1",
                "auto_reply",
                0.9,
                "j",
                "t",
                None,
                None,
                Some("agent-skippy"),
            )
            .unwrap();
        let stored: Option<String> = db
            .conn()
            .query_row(
                "SELECT agent_id FROM action_log WHERE id = ?1",
                super::params![attributed.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_deref(), Some("agent-skippy"));

        // Event-linked attributed writer.
        let ev = db
            .log_action_for_event_with_agent(
                super::EventActionLogInput {
                    account_id: "acc-1",
                    event_id: "evt-77",
                    action_type: "mark_handled",
                    confidence: 1.0,
                    justification: "j",
                    action_taken: "t",
                    action_status: "completed",
                    message_id: None,
                    draft_id: None,
                },
                Some("agent-bravo"),
            )
            .unwrap();
        let ev_agent: Option<String> = db
            .conn()
            .query_row(
                "SELECT agent_id FROM action_log WHERE id = ?1",
                super::params![ev.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ev_agent.as_deref(), Some("agent-bravo"));
    }

    #[test]
    fn list_actions_for_agent_filters_by_agent_id() {
        let db = Database::open_memory().unwrap();
        db.log_action_with_agent("acc-1", "move", 1.0, "j", "t", None, None, Some("agent-a"))
            .unwrap();
        db.log_action_with_agent("acc-1", "flag", 1.0, "j", "t", None, None, Some("agent-b"))
            .unwrap();
        // Legacy (no agent) row must not match any agent filter.
        db.log_action("acc-1", "classify", 0.5, "j", "t", None, None)
            .unwrap();

        let a = db.list_actions_for_agent("acc-1", "agent-a", 10).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].action_type, "move");

        let b = db.list_actions_for_agent("acc-1", "agent-b", 10).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].action_type, "flag");

        // Unknown agent yields nothing.
        assert!(
            db.list_actions_for_agent("acc-1", "agent-z", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn log_action_for_event_is_idempotent() {
        let db = Database::open_memory().unwrap();

        let first = db
            .log_action_for_event(super::EventActionLogInput {
                account_id: "acc-test-1",
                event_id: "evt-1",
                action_type: "mark_handled",
                confidence: 1.0,
                justification: "agent callback",
                action_taken: "marked handled",
                action_status: "completed",
                message_id: Some("<msg-123@example.com>"),
                draft_id: None,
            })
            .unwrap();
        let second = db
            .log_action_for_event(super::EventActionLogInput {
                account_id: "acc-test-1",
                event_id: "evt-1",
                action_type: "mark_handled",
                confidence: 1.0,
                justification: "agent callback",
                action_taken: "marked handled",
                action_status: "completed",
                message_id: Some("<msg-123@example.com>"),
                draft_id: None,
            })
            .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(first.event_id.as_deref(), Some("evt-1"));
        assert_eq!(first.action_status, "completed");
        assert_eq!(db.list_actions("acc-test-1", 10).unwrap().len(), 1);
    }

    #[test]
    fn log_action_for_event_preserves_first_local_audit_payload() {
        let db = Database::open_memory().unwrap();

        let first = db
            .log_action_for_event(super::EventActionLogInput {
                account_id: "acc-test-1",
                event_id: "evt-9",
                action_type: "mark_handled",
                confidence: 1.0,
                justification: "mark-handled executed locally; no mailbox mutation",
                action_taken: r#"{"kind":"mark_handled","actor":"agent-a","mode":"local_audit_only"}"#,
                action_status: "completed",
                message_id: Some("<msg-9@example.com>"),
                draft_id: None,
            })
            .unwrap();
        let second = db
            .log_action_for_event(super::EventActionLogInput {
                account_id: "acc-test-1",
                event_id: "evt-9",
                action_type: "mark_handled",
                confidence: 1.0,
                justification: "mark-handled executed locally; no mailbox mutation",
                action_taken: r#"{"kind":"mark_handled","actor":"agent-b","mode":"local_audit_only"}"#,
                action_status: "completed",
                message_id: Some("<msg-9@example.com>"),
                draft_id: None,
            })
            .unwrap();

        assert_eq!(first.id, second.id);
        assert!(first.action_taken.contains("\"actor\":\"agent-a\""));
        assert_eq!(first.action_taken, second.action_taken);
    }
}
