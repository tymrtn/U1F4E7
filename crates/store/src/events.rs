// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::Event;
use rusqlite::{OptionalExtension, params};

/// Filters for the Logs read ([`Database::list_event_log`]). Every field is
/// optional; `limit` counts entries after collapse, not raw rows.
#[derive(Debug, Default, Clone)]
pub struct EventLogFilter {
    pub account_id: Option<String>,
    /// An exact event type, or a dotted prefix: `send_governor` matches
    /// `send_governor.blocked` and `send_governor.allowed`.
    pub event_type: Option<String>,
    /// Inclusive lower bound on `created_at`.
    pub since: Option<String>,
    /// Exclusive upper bound on `created_at` — the paging cursor.
    pub before: Option<String>,
    pub limit: usize,
}

/// One Logs entry: the newest event of a same-day run of the same event about
/// the same draft (or message), plus what that run collapsed.
#[derive(Debug, Clone)]
pub struct EventLogEntry {
    pub event: Event,
    pub agent_id: Option<String>,
    /// From the payload (`$.draft_id` or `$.request.draft_id`), when the event
    /// concerns a draft.
    pub draft_id: Option<String>,
    pub repeat_count: i64,
    pub first_at: String,
}

impl Database {
    /// Insert an event into the events table. Attribution is human/legacy
    /// (agent_id stored as NULL); use [`Database::insert_event_with_agent`] to
    /// attribute the event to an agent.
    pub fn insert_event(&self, event: &Event) -> Result<()> {
        self.insert_event_with_agent(event, None)
    }

    /// Insert an event attributed to a specific agent. `agent_id` is `None` for
    /// human/legacy events (stored as NULL).
    pub fn insert_event_with_agent(&self, event: &Event, agent_id: Option<&str>) -> Result<()> {
        self.conn().execute(
            "INSERT INTO events (
                id, account_id, event_type, folder, uid, message_id, from_addr, subject, snippet,
                payload, idempotency_key, secure_pending, acked_at, created_at, agent_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                event.id,
                event.account_id,
                event.event_type,
                event.folder,
                event.uid,
                crate::message_ids::stored_message_id(event.message_id.as_deref()),
                event.from_addr,
                event.subject,
                event.snippet,
                event.payload,
                event.idempotency_key,
                event.secure_pending,
                event.acked_at,
                event.created_at,
                agent_id,
            ],
        )?;
        Ok(())
    }

    /// Insert an event, ignoring duplicates guarded by the idempotency key.
    /// Attribution is human/legacy (agent_id NULL).
    pub fn insert_event_idempotent(&self, event: &Event) -> Result<bool> {
        self.insert_event_idempotent_with_agent(event, None)
    }

    /// Idempotent insert attributed to a specific agent. `agent_id` is `None`
    /// for human/legacy events (stored as NULL).
    pub fn insert_event_idempotent_with_agent(
        &self,
        event: &Event,
        agent_id: Option<&str>,
    ) -> Result<bool> {
        let inserted = self.conn().execute(
            "INSERT OR IGNORE INTO events (
                id, account_id, event_type, folder, uid, message_id, from_addr, subject, snippet,
                payload, idempotency_key, secure_pending, acked_at, created_at, agent_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                event.id,
                event.account_id,
                event.event_type,
                event.folder,
                event.uid,
                crate::message_ids::stored_message_id(event.message_id.as_deref()),
                event.from_addr,
                event.subject,
                event.snippet,
                event.payload,
                event.idempotency_key,
                event.secure_pending,
                event.acked_at,
                event.created_at,
                agent_id,
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

    /// The Logs read: newest first, with same-day runs of one event type about
    /// one draft (or message) collapsed into a single entry. The scheduled
    /// sweep re-evaluates a blocked draft every cooldown and writes a
    /// `send_governor.blocked` row each time — 22k rows for one July draft on
    /// the reference install — so without this the page is one draft repeated.
    /// Runs never merge across days, so "blocked again next week" stays its own
    /// entry. The bare columns ride along with `MAX(created_at)`, which SQLite
    /// guarantees come from that newest row.
    ///
    /// The catalog `governor_blocked` event is left out: its only writer
    /// (`record_governor_event` in the CLI gate) writes it right after the
    /// `send_governor.blocked` audit row for the same block, so that delivery
    /// routes can subscribe by a stable name. Listing both showed every block
    /// twice.
    pub fn list_event_log(&self, filter: &EventLogFilter) -> Result<Vec<EventLogEntry>> {
        let type_prefix = filter.event_type.as_ref().map(|t| format!("{t}.%"));
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, event_type, folder, uid, message_id, from_addr, subject,
                    snippet, payload, idempotency_key, secure_pending, acked_at,
                    MAX(created_at) AS last_at,
                    agent_id,
                    COALESCE(json_extract(payload, '$.draft_id'),
                             json_extract(payload, '$.request.draft_id')) AS draft_id,
                    COUNT(*) AS repeat_count,
                    MIN(created_at) AS first_at
             FROM events
             WHERE (?1 IS NULL OR account_id = ?1)
               AND (?2 IS NULL OR event_type = ?2 OR event_type LIKE ?3)
               AND (?4 IS NULL OR created_at >= ?4)
               AND (?5 IS NULL OR created_at < ?5)
               AND event_type <> ?7
             GROUP BY account_id, event_type,
                      COALESCE(json_extract(payload, '$.draft_id'),
                               json_extract(payload, '$.request.draft_id'),
                               message_id, id),
                      substr(created_at, 1, 10)
             ORDER BY last_at DESC
             LIMIT ?6",
        )?;
        let rows = stmt.query_map(
            params![
                filter.account_id,
                filter.event_type,
                type_prefix,
                filter.since,
                filter.before,
                filter.limit as i64,
                crate::event_catalog::GOVERNOR_BLOCKED
            ],
            |row| {
                Ok(EventLogEntry {
                    event: map_event(row)?,
                    agent_id: row.get(14)?,
                    draft_id: row.get(15)?,
                    repeat_count: row.get(16)?,
                    first_at: row.get(17)?,
                })
            },
        )?;
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

    /// True totals of unacked events for an account, excluding the given
    /// event types, split by message anchor: `(anchored, bare)` where anchored
    /// rows carry a uid. The uncapped companion to [`Database::list_unacked`],
    /// so the review queue's capped item lists can report whole-queue counts.
    pub fn count_unacked_by_anchor(
        &self,
        account_id: &str,
        exclude_event_types: &[&str],
    ) -> Result<(i64, i64)> {
        let mut sql = String::from(
            "SELECT
                COALESCE(SUM(CASE WHEN uid IS NOT NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN uid IS NULL THEN 1 ELSE 0 END), 0)
             FROM events
             WHERE account_id = ?1 AND acked_at IS NULL",
        );
        if !exclude_event_types.is_empty() {
            let placeholders = (0..exclude_event_types.len())
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(" AND event_type NOT IN ({placeholders})"));
        }
        let params_iter = std::iter::once(account_id).chain(exclude_event_types.iter().copied());
        let mut stmt = self.conn().prepare(&sql)?;
        Ok(
            stmt.query_row(rusqlite::params_from_iter(params_iter), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?,
        )
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

    /// Newest `event_type` event about one message (by canonical Message-ID).
    pub fn latest_event_for_message(
        &self,
        account_id: &str,
        event_type: &str,
        message_id: &str,
    ) -> Result<Option<Event>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, event_type, folder, uid, message_id, from_addr, subject,
                    snippet, payload, idempotency_key, secure_pending, acked_at, created_at
             FROM events
             WHERE account_id = ?1 AND event_type = ?2 AND message_id = ?3
             ORDER BY created_at DESC, rowid DESC
             LIMIT 1",
        )?;
        Ok(stmt
            .query_row(params![account_id, event_type, message_id], map_event)
            .optional()?)
    }

    /// Newest `event_type` event recorded for a folder/UID.
    pub fn latest_event_for_uid(
        &self,
        account_id: &str,
        event_type: &str,
        folder: &str,
        uid: u32,
    ) -> Result<Option<Event>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, event_type, folder, uid, message_id, from_addr, subject,
                    snippet, payload, idempotency_key, secure_pending, acked_at, created_at
             FROM events
             WHERE account_id = ?1 AND event_type = ?2 AND folder = ?3 AND uid = ?4
             ORDER BY created_at DESC, rowid DESC
             LIMIT 1",
        )?;
        Ok(stmt
            .query_row(
                params![account_id, event_type, folder, i64::from(uid)],
                map_event,
            )
            .optional()?)
    }

    /// Payload of the newest `event_type` event per message (keyed by
    /// Message-ID, else folder/UID). SQLite takes the bare `payload` column
    /// from the `MAX(created_at)` row of each group.
    pub fn latest_event_payloads_per_message(
        &self,
        account_id: &str,
        event_type: &str,
    ) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare(
            "SELECT payload, MAX(created_at)
             FROM events
             WHERE account_id = ?1 AND event_type = ?2 AND payload IS NOT NULL
             GROUP BY COALESCE(message_id, folder || ':' || uid)",
        )?;
        let rows = stmt.query_map(params![account_id, event_type], |row| {
            row.get::<_, String>(0)
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Fetch the event recorded under an idempotency key, if any.
    pub fn get_event_by_idempotency_key(&self, key: &str) -> Result<Option<Event>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, event_type, folder, uid, message_id, from_addr, subject,
                    snippet, payload, idempotency_key, secure_pending, acked_at, created_at
             FROM events
             WHERE idempotency_key = ?1",
        )?;
        Ok(stmt.query_row(params![key], map_event).optional()?)
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
    fn a_governor_block_is_one_logs_entry() {
        // The CLI/MCP gate writes the `send_governor.blocked` audit row, then
        // the catalog `governor_blocked` row for delivery routes. Both record
        // the same block; Logs shows it once.
        let db = test_db();
        let outcome = serde_json::json!({ "allowed": false, "block_code": "governor_blocked" });
        let audit = Event {
            id: "audit-1".to_string(),
            account_id: "acc-1".to_string(),
            event_type: "send_governor.blocked".to_string(),
            folder: "policy".to_string(),
            uid: None,
            message_id: None,
            from_addr: None,
            subject: None,
            snippet: None,
            payload: Some(
                serde_json::json!({ "request": { "draft_id": "d1" }, "outcome": outcome })
                    .to_string(),
            ),
            idempotency_key: None,
            secure_pending: false,
            acked_at: Some("2026-09-17T21:17:13+00:00".to_string()),
            created_at: "2026-09-17T21:17:13+00:00".to_string(),
        };
        db.insert_event_with_agent(&audit, Some("agent-skippy"))
            .unwrap();
        db.emit_catalog_event(
            "acc-1",
            crate::event_catalog::GOVERNOR_BLOCKED,
            Some(serde_json::json!({ "outcome": outcome })),
            Some("agent-skippy"),
        )
        .unwrap();

        let entries = db
            .list_event_log(&EventLogFilter {
                limit: 10,
                ..EventLogFilter::default()
            })
            .unwrap();
        let types: Vec<&str> = entries
            .iter()
            .map(|e| e.event.event_type.as_str())
            .collect();
        assert_eq!(types, ["send_governor.blocked"]);
        assert!(
            db.list_events(Some("acc-1"), 10)
                .unwrap()
                .iter()
                .any(|e| e.event_type == crate::event_catalog::GOVERNOR_BLOCKED),
            "delivery routes still see the catalog event"
        );
    }

    #[test]
    fn agent_id_lands_and_legacy_insert_is_null() {
        let db = test_db();
        let base = Event {
            id: "evt-legacy".to_string(),
            account_id: "acc-1".to_string(),
            event_type: "new_message".to_string(),
            folder: "INBOX".to_string(),
            uid: None,
            message_id: None,
            from_addr: None,
            subject: None,
            snippet: None,
            payload: None,
            idempotency_key: Some("k-legacy".to_string()),
            secure_pending: false,
            acked_at: None,
            created_at: "2026-04-19T12:00:00".to_string(),
        };

        db.insert_event(&base).unwrap();
        let legacy_agent: Option<String> = db
            .conn()
            .query_row(
                "SELECT agent_id FROM events WHERE id = 'evt-legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_agent, None);

        db.insert_event_with_agent(
            &Event {
                id: "evt-agent".to_string(),
                idempotency_key: Some("k-agent".to_string()),
                ..base.clone()
            },
            Some("agent-skippy"),
        )
        .unwrap();
        let attributed: Option<String> = db
            .conn()
            .query_row(
                "SELECT agent_id FROM events WHERE id = 'evt-agent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attributed.as_deref(), Some("agent-skippy"));

        assert!(
            db.insert_event_idempotent_with_agent(
                &Event {
                    id: "evt-idem".to_string(),
                    idempotency_key: Some("k-idem".to_string()),
                    ..base.clone()
                },
                Some("agent-bravo"),
            )
            .unwrap()
        );
        let idem_agent: Option<String> = db
            .conn()
            .query_row(
                "SELECT agent_id FROM events WHERE id = 'evt-idem'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idem_agent.as_deref(), Some("agent-bravo"));
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
