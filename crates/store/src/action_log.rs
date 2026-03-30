// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::ActionLog;
use rusqlite::params;
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
        let id = Uuid::new_v4().to_string();

        self.conn().execute(
            "INSERT INTO action_log (id, account_id, action_type, confidence, justification,
             action_taken, message_id, draft_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                account_id,
                action_type,
                confidence,
                justification,
                action_taken,
                message_id,
                draft_id
            ],
        )?;

        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, action_type, confidence, justification, action_taken,
                    message_id, draft_id, created_at
             FROM action_log WHERE id = ?1",
        )?;

        let action = stmt.query_row(params![id], |row| {
            Ok(ActionLog {
                id: row.get(0)?,
                account_id: row.get(1)?,
                action_type: row.get(2)?,
                confidence: row.get(3)?,
                justification: row.get(4)?,
                action_taken: row.get(5)?,
                message_id: row.get(6)?,
                draft_id: row.get(7)?,
                created_at: row.get(8)?,
            })
        })?;

        Ok(action)
    }

    /// List recent actions for an account, newest first.
    pub fn list_actions(&self, account_id: &str, limit: u32) -> Result<Vec<ActionLog>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, action_type, confidence, justification, action_taken,
                    message_id, draft_id, created_at
             FROM action_log WHERE account_id = ?1
             ORDER BY created_at DESC LIMIT ?2",
        )?;

        let actions = stmt
            .query_map(params![account_id, limit], |row| {
                Ok(ActionLog {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    action_type: row.get(2)?,
                    confidence: row.get(3)?,
                    justification: row.get(4)?,
                    action_taken: row.get(5)?,
                    message_id: row.get(6)?,
                    draft_id: row.get(7)?,
                    created_at: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(actions)
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Database;

    #[test]
    fn log_and_list_actions() {
        let db = Database::open_memory().unwrap();

        // Insert a dummy account for the foreign-key-free action_log
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

        // Log a second action
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
        // Newest first
        assert_eq!(actions[0].action_type, "classify");
        assert_eq!(actions[1].action_type, "auto_reply");
    }
}
