// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Persistence for the durable event-delivery pipeline (v2 webhook push).
//!
//! A *delivery* is one attempt-tracked association between a stored event and
//! an [`crate::models::EventRoute`]. When a watch emits an event, one delivery
//! row is enqueued per matching route. The delivery executor (in the transport
//! crate) then drives each row through an at-least-once retry schedule until it
//! is either delivered (a 2xx response) or dead-lettered (retries exhausted).
//!
//! Terminal states are recorded by nullable timestamps rather than a status
//! string so that the meaning of legacy rows is preserved:
//!   * `delivered_at IS NOT NULL`      → delivered, terminal.
//!   * `dead_lettered_at IS NOT NULL`  → gave up after N attempts, terminal.
//!   * both NULL                       → pending; `next_attempt_at` gates when.

use crate::db::Database;
use crate::errors::Result;
use crate::models::EventDelivery;
use rusqlite::{OptionalExtension, params};

/// Maximum stored size, in bytes, of a captured HTTP response body. Larger
/// bodies are truncated at this boundary before storage to bound the DB and to
/// avoid persisting unbounded remote content. Callers should truncate on a
/// char boundary; this constant is the hard cap.
pub const RESPONSE_SNIPPET_CAP_BYTES: usize = 1024;

/// Columns selected for every [`EventDelivery`] read, in struct-field order.
const DELIVERY_COLUMNS: &str = "id, event_id, route_id, delivery_id, status, attempt_count, \
     last_attempt_at, error_summary, next_attempt_at, last_status_code, \
     last_response_snippet, last_error, dead_lettered_at, delivered_at, created_at";

/// Filter for [`Database::list_deliveries`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatusFilter {
    /// Not yet delivered and not dead-lettered.
    Pending,
    /// Retries exhausted.
    Dead,
    /// Successfully delivered.
    Delivered,
    /// No filter.
    All,
}

impl Database {
    /// Enqueue a pending delivery for an (event, route) pair. Idempotent on the
    /// `(event_id, route_id, delivery_id)` unique key — a re-enqueue for an
    /// already-known delivery is a no-op and returns `false`.
    pub fn enqueue_delivery(
        &self,
        id: &str,
        event_id: &str,
        route_id: &str,
        delivery_id: &str,
        next_attempt_at: &str,
    ) -> Result<bool> {
        let inserted = self.conn().execute(
            "INSERT OR IGNORE INTO event_deliveries
                (id, event_id, route_id, delivery_id, status, next_attempt_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            params![id, event_id, route_id, delivery_id, next_attempt_at],
        )?;
        Ok(inserted > 0)
    }

    /// Fetch a single delivery by id.
    pub fn get_delivery(&self, id: &str) -> Result<Option<EventDelivery>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {DELIVERY_COLUMNS} FROM event_deliveries WHERE id = ?1"
        ))?;
        Ok(stmt.query_row(params![id], map_delivery).optional()?)
    }

    /// Deliveries that are due now: pending (neither delivered nor dead-lettered)
    /// and whose `next_attempt_at` is at or before `now`. Oldest attempt first.
    pub fn list_due_deliveries(&self, now: &str, limit: usize) -> Result<Vec<EventDelivery>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {DELIVERY_COLUMNS} FROM event_deliveries
             WHERE delivered_at IS NULL
               AND dead_lettered_at IS NULL
               AND (next_attempt_at IS NULL OR datetime(next_attempt_at) <= datetime(?1))
             ORDER BY COALESCE(next_attempt_at, created_at) ASC
             LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![now, limit as i64], map_delivery)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// List deliveries filtered by high-level status.
    pub fn list_deliveries(
        &self,
        filter: DeliveryStatusFilter,
        limit: usize,
    ) -> Result<Vec<EventDelivery>> {
        let predicate = match filter {
            DeliveryStatusFilter::Pending => "delivered_at IS NULL AND dead_lettered_at IS NULL",
            DeliveryStatusFilter::Dead => "dead_lettered_at IS NOT NULL",
            DeliveryStatusFilter::Delivered => "delivered_at IS NOT NULL",
            DeliveryStatusFilter::All => "1 = 1",
        };
        let sql = format!(
            "SELECT {DELIVERY_COLUMNS} FROM event_deliveries
             WHERE {predicate}
             ORDER BY created_at DESC
             LIMIT ?1"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![limit as i64], map_delivery)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Record a successful (2xx) delivery attempt. Terminal.
    pub fn record_delivery_success(
        &self,
        id: &str,
        status_code: u16,
        response_snippet: Option<&str>,
        now: &str,
    ) -> Result<()> {
        let snippet = response_snippet.map(cap_snippet);
        self.conn().execute(
            "UPDATE event_deliveries SET
                status = 'delivered',
                attempt_count = attempt_count + 1,
                last_attempt_at = ?2,
                last_status_code = ?3,
                last_response_snippet = ?4,
                last_error = NULL,
                delivered_at = ?2
             WHERE id = ?1",
            params![id, now, status_code as i64, snippet],
        )?;
        Ok(())
    }

    /// Record a failed delivery attempt. When `next_attempt_at` is `Some`, the
    /// delivery stays pending and becomes due again at that time. When it is
    /// `None`, retries are exhausted and the delivery is dead-lettered at `now`.
    pub fn record_delivery_failure(
        &self,
        id: &str,
        status_code: Option<u16>,
        response_snippet: Option<&str>,
        error: Option<&str>,
        next_attempt_at: Option<&str>,
        now: &str,
    ) -> Result<()> {
        let snippet = response_snippet.map(cap_snippet);
        let dead_lettered_at = if next_attempt_at.is_none() {
            Some(now)
        } else {
            None
        };
        let status = if dead_lettered_at.is_some() {
            "dead"
        } else {
            "pending"
        };
        self.conn().execute(
            "UPDATE event_deliveries SET
                status = ?7,
                attempt_count = attempt_count + 1,
                last_attempt_at = ?2,
                last_status_code = ?3,
                last_response_snippet = ?4,
                last_error = ?5,
                error_summary = ?5,
                next_attempt_at = ?6,
                dead_lettered_at = ?8
             WHERE id = ?1",
            params![
                id,
                now,
                status_code.map(|c| c as i64),
                snippet,
                error,
                next_attempt_at,
                status,
                dead_lettered_at,
            ],
        )?;
        Ok(())
    }

    /// Clear the dead-letter and backoff state so a delivery is retried on the
    /// next executor pass. Returns `false` if no such delivery exists. A
    /// delivery already delivered is left untouched (returns `false`).
    pub fn reset_delivery_for_retry(&self, id: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE event_deliveries SET
                status = 'pending',
                dead_lettered_at = NULL,
                next_attempt_at = NULL,
                last_error = NULL
             WHERE id = ?1 AND delivered_at IS NULL",
            params![id],
        )? > 0)
    }
}

/// Truncate a captured response body to [`RESPONSE_SNIPPET_CAP_BYTES`] on a
/// char boundary so stored snippets never exceed the documented cap.
pub fn cap_snippet(body: &str) -> String {
    if body.len() <= RESPONSE_SNIPPET_CAP_BYTES {
        return body.to_string();
    }
    let mut end = RESPONSE_SNIPPET_CAP_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    body[..end].to_string()
}

fn map_delivery(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventDelivery> {
    Ok(EventDelivery {
        id: row.get(0)?,
        event_id: row.get(1)?,
        route_id: row.get(2)?,
        delivery_id: row.get(3)?,
        status: row.get(4)?,
        attempt_count: row.get(5)?,
        last_attempt_at: row.get(6)?,
        error_summary: row.get(7)?,
        next_attempt_at: row.get(8)?,
        last_status_code: row.get(9)?,
        last_response_snippet: row.get(10)?,
        last_error: row.get(11)?,
        dead_lettered_at: row.get(12)?,
        delivered_at: row.get(13)?,
        created_at: row.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_route(db: &Database) -> String {
        db.create_event_route(
            "acc-1",
            r#"{"event_types":["new_message"]}"#,
            r#"{"type":"webhook","url":"https://example.test/hook"}"#,
            true,
            100,
        )
        .unwrap()
        .id
    }

    #[test]
    fn enqueue_is_idempotent_on_delivery_key() {
        let db = Database::open_memory().unwrap();
        let route = seed_route(&db);
        assert!(
            db.enqueue_delivery("d1", "evt-1", &route, "del-1", "2026-07-08T00:00:00")
                .unwrap()
        );
        // Same (event, route, delivery) → no second row.
        assert!(
            !db.enqueue_delivery("d2", "evt-1", &route, "del-1", "2026-07-08T00:00:00")
                .unwrap()
        );
        assert_eq!(
            db.list_deliveries(DeliveryStatusFilter::All, 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn due_query_respects_backoff_and_terminal_states() {
        let db = Database::open_memory().unwrap();
        let route = seed_route(&db);
        db.enqueue_delivery("d-due", "e1", &route, "x1", "2026-07-08T00:00:00")
            .unwrap();
        db.enqueue_delivery("d-future", "e2", &route, "x2", "2999-01-01T00:00:00")
            .unwrap();
        db.enqueue_delivery("d-done", "e3", &route, "x3", "2026-07-08T00:00:00")
            .unwrap();
        db.record_delivery_success("d-done", 200, Some("ok"), "2026-07-08T00:05:00")
            .unwrap();

        let due = db.list_due_deliveries("2026-07-08T00:10:00", 50).unwrap();
        let ids: Vec<_> = due.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["d-due"],
            "only the past-due pending row is returned"
        );
    }

    #[test]
    fn cap_snippet_truncates_at_one_kib_on_char_boundary() {
        let big = "é".repeat(1024); // 2048 bytes
        let capped = cap_snippet(&big);
        assert!(capped.len() <= RESPONSE_SNIPPET_CAP_BYTES);
        assert!(big.starts_with(&capped));
    }
}
