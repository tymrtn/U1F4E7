// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::Event;
use rusqlite::{OptionalExtension, params};

impl Database {
    /// Insert an event into the events table.
    pub fn insert_event(&self, event: &Event) -> Result<()> {
        self.conn().execute(
            "INSERT INTO events (
                id, account_id, event_type, folder, uid, message_id, from_addr, subject, snippet,
                payload, idempotency_key, secure_pending, acked_at, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                event.id,
                event.account_id,
                event.event_type,
                event.folder,
                event.uid,
                event.message_id,
                event.from_addr,
                event.subject,
                event.snippet,
                event.payload,
                event.idempotency_key,
                event.secure_pending,
                event.acked_at,
                event.created_at,
            ],
        )?;
        Ok(())
    }

    /// Insert an event, ignoring duplicates guarded by the idempotency key.
    pub fn insert_event_idempotent(&self, event: &Event) -> Result<bool> {
        let inserted = self.conn().execute(
            "INSERT OR IGNORE INTO events (
                id, account_id, event_type, folder, uid, message_id, from_addr, subject, snippet,
                payload, idempotency_key, secure_pending, acked_at, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                event.id,
                event.account_id,
                event.event_type,
                event.folder,
                event.uid,
                event.message_id,
                event.from_addr,
                event.subject,
                event.snippet,
                event.payload,
                event.idempotency_key,
                event.secure_pending,
                event.acked_at,
                event.created_at,
            ],
        )?;
        Ok(inserted > 0)
    }

    /// List recent events, optionally filtered by account.
    pub fn list_events(&self, account_id: Option<&str>, limit: usize) -> Result<Vec<Event>> {
        let (sql, query_params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match account_id {
            Some(id) => (
                "SELECT id, account_id, event_type, folder, uid, message_id, from_addr, subject,
                        snippet, payload, idempotency_key, secure_pending, acked_at, created_at
                 FROM events
                 WHERE account_id = ?1
                 ORDER BY created_at DESC
                 LIMIT ?2",
                vec![Box::new(id.to_string()), Box::new(limit as i64)],
            ),
            None => (
                "SELECT id, account_id, event_type, folder, uid, message_id, from_addr, subject,
                        snippet, payload, idempotency_key, secure_pending, acked_at, created_at
                 FROM events
                 ORDER BY created_at DESC
                 LIMIT ?1",
                vec![Box::new(limit as i64)],
            ),
        };

        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(query_params.iter()), map_event)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Check if there are recent events (within last N seconds).
    /// Used by `envelope code` to decide whether to tail events or poll IMAP.
    pub fn has_recent_events(&self, seconds: i64) -> Result<bool> {
        let count: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM events WHERE created_at >= datetime('now', ?1)",
            params![format!("-{seconds} seconds")],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// List events newer than a given timestamp for a specific account.
    pub fn list_events_since(&self, account_id: &str, since: &str) -> Result<Vec<Event>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, event_type, folder, uid, message_id, from_addr, subject,
                    snippet, payload, idempotency_key, secure_pending, acked_at, created_at
             FROM events
             WHERE account_id = ?1 AND created_at > ?2
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![account_id, since], map_event)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Mark an event as acknowledged.
    pub fn mark_acked(&self, event_id: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE events
             SET acked_at = COALESCE(acked_at, datetime('now'))
             WHERE id = ?1",
            params![event_id],
        )? > 0)
    }

    /// Fetch unacked events for an account, oldest first.
    pub fn list_unacked(&self, account_id: &str, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, event_type, folder, uid, message_id, from_addr, subject,
                    snippet, payload, idempotency_key, secure_pending, acked_at, created_at
             FROM events
             WHERE account_id = ?1 AND acked_at IS NULL
             ORDER BY created_at ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![account_id, limit as i64], map_event)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Fetch a single event by id.
    pub fn get_event(&self, event_id: &str) -> Result<Option<Event>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, event_type, folder, uid, message_id, from_addr, subject,
                    snippet, payload, idempotency_key, secure_pending, acked_at, created_at
             FROM events
             WHERE id = ?1",
        )?;
        Ok(stmt.query_row(params![event_id], map_event).optional()?)
    }

    /// Prune events older than N days.
    pub fn prune_events(&self, days: i64) -> Result<usize> {
        let deleted = self.conn().execute(
            "DELETE FROM events WHERE created_at < datetime('now', ?1)",
            params![format!("-{days} days")],
        )?;
        Ok(deleted)
    }
}

fn map_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: row.get(0)?,
        account_id: row.get(1)?,
        event_type: row.get(2)?,
        folder: row.get(3)?,
        uid: row.get(4)?,
        message_id: row.get(5)?,
        from_addr: row.get(6)?,
        subject: row.get(7)?,
        snippet: row.get(8)?,
        payload: row.get(9)?,
        idempotency_key: row.get(10)?,
        secure_pending: row.get(11)?,
        acked_at: row.get(12)?,
        created_at: row.get(13)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::open_memory().unwrap()
    }

    #[test]
    fn insert_and_list_events() {
        let db = test_db();
        let event = Event {
            id: "evt-1".to_string(),
            account_id: "acc-1".to_string(),
            event_type: "new_message".to_string(),
            folder: "INBOX".to_string(),
            uid: Some(42),
            message_id: Some("<msg@example.com>".to_string()),
            from_addr: Some("alice@example.com".to_string()),
            subject: Some("Hello".to_string()),
            snippet: Some("Hi there...".to_string()),
            payload: None,
            idempotency_key: Some("idem-1".to_string()),
            secure_pending: false,
            acked_at: None,
            created_at: "2026-04-19T12:00:00".to_string(),
        };
        db.insert_event(&event).unwrap();

        let events = db.list_events(Some("acc-1"), 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "new_message");
        assert_eq!(events[0].uid, Some(42));
    }

    #[test]
    fn list_events_filters_by_account() {
        let db = test_db();
        for (i, acc) in ["acc-1", "acc-2"].iter().enumerate() {
            db.insert_event(&Event {
                id: format!("evt-{i}"),
                account_id: acc.to_string(),
                event_type: "new_message".to_string(),
                folder: "INBOX".to_string(),
                uid: Some(i as i64),
                message_id: None,
                from_addr: None,
                subject: None,
                snippet: None,
                payload: None,
                idempotency_key: Some(format!("idem-{i}")),
                secure_pending: false,
                acked_at: None,
                created_at: "2026-04-19T12:00:00".to_string(),
            })
            .unwrap();
        }

        assert_eq!(db.list_events(Some("acc-1"), 10).unwrap().len(), 1);
        assert_eq!(db.list_events(None, 10).unwrap().len(), 2);
    }

    #[test]
    fn insert_event_idempotent_deduplicates_by_account_and_key() {
        let db = test_db();
        let base = Event {
            id: "evt-1".to_string(),
            account_id: "acc-1".to_string(),
            event_type: "otp_detected".to_string(),
            folder: "INBOX".to_string(),
            uid: Some(42),
            message_id: Some("<msg@example.com>".to_string()),
            from_addr: Some("alice@example.com".to_string()),
            subject: Some("Your code is 123456".to_string()),
            snippet: Some("Use code 123456".to_string()),
            payload: Some(r#"{"confidence":0.95}"#.to_string()),
            idempotency_key: Some("same-key".to_string()),
            secure_pending: true,
            acked_at: None,
            created_at: "2026-04-19T12:00:00".to_string(),
        };

        assert!(db.insert_event_idempotent(&base).unwrap());
        assert!(
            !db.insert_event_idempotent(&Event {
                id: "evt-2".to_string(),
                ..base.clone()
            })
            .unwrap()
        );
        assert!(
            db.insert_event_idempotent(&Event {
                id: "evt-3".to_string(),
                account_id: "acc-2".to_string(),
                ..base.clone()
            })
            .unwrap()
        );

        assert_eq!(db.list_events(None, 10).unwrap().len(), 2);
    }

    #[test]
    fn list_unacked_and_mark_acked() {
        let db = test_db();
        for (id, acked_at) in [("evt-1", None), ("evt-2", Some("2026-04-19T12:05:00"))] {
            db.insert_event(&Event {
                id: id.to_string(),
                account_id: "acc-1".to_string(),
                event_type: "new_message".to_string(),
                folder: "INBOX".to_string(),
                uid: None,
                message_id: None,
                from_addr: None,
                subject: None,
                snippet: None,
                payload: None,
                idempotency_key: Some(format!("key-{id}")),
                secure_pending: false,
                acked_at: acked_at.map(str::to_string),
                created_at: "2026-04-19T12:00:00".to_string(),
            })
            .unwrap();
        }

        let unacked = db.list_unacked("acc-1", 10).unwrap();
        assert_eq!(unacked.len(), 1);
        assert_eq!(unacked[0].id, "evt-1");

        assert!(db.mark_acked("evt-1").unwrap());
        let fetched = db.get_event("evt-1").unwrap().unwrap();
        assert!(fetched.acked_at.is_some());
        assert!(db.list_unacked("acc-1", 10).unwrap().is_empty());
    }
}
