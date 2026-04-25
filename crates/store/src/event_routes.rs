// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::EventRoute;
use rusqlite::params;
use uuid::Uuid;

impl Database {
    /// Create or replace an event route.
    pub fn upsert_event_route(
        &self,
        account_id: &str,
        match_expr: &str,
        delivery: &str,
        enabled: bool,
        priority: i64,
        route_id: Option<&str>,
    ) -> Result<EventRoute> {
        let id = route_id
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        self.conn().execute(
            "INSERT INTO event_routes (id, account_id, match_expr, delivery, enabled, priority)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                account_id = excluded.account_id,
                match_expr = excluded.match_expr,
                delivery = excluded.delivery,
                enabled = excluded.enabled,
                priority = excluded.priority,
                updated_at = datetime('now')",
            params![id, account_id, match_expr, delivery, enabled, priority],
        )?;
        self.get_event_route(&id)
    }

    /// Fetch a single event route by id.
    pub fn get_event_route(&self, route_id: &str) -> Result<EventRoute> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, match_expr, delivery, enabled, priority, created_at, updated_at
             FROM event_routes
             WHERE id = ?1",
        )?;
        Ok(stmt.query_row(params![route_id], map_event_route)?)
    }

    /// List event routes for an account in priority order.
    pub fn list_event_routes(&self, account_id: &str) -> Result<Vec<EventRoute>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, match_expr, delivery, enabled, priority, created_at, updated_at
             FROM event_routes
             WHERE account_id = ?1
             ORDER BY priority ASC, created_at ASC",
        )?;
        let rows = stmt.query_map(params![account_id], map_event_route)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Delete an event route by id.
    pub fn delete_event_route(&self, route_id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM event_routes WHERE id = ?1", params![route_id])?
            > 0)
    }
}

fn map_event_route(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRoute> {
    Ok(EventRoute {
        id: row.get(0)?,
        account_id: row.get(1)?,
        match_expr: row.get(2)?,
        delivery: row.get(3)?,
        enabled: row.get(4)?,
        priority: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::db::Database;

    #[test]
    fn event_route_crud() {
        let db = Database::open_memory().unwrap();
        let route = db
            .upsert_event_route(
                "acc-1",
                r#"{"kind":"otp_detected"}"#,
                r#"[{"type":"stdout"}]"#,
                true,
                50,
                None,
            )
            .unwrap();

        let listed = db.list_event_routes("acc-1").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, route.id);

        let updated = db
            .upsert_event_route(
                "acc-1",
                r#"{"kind":"new_message"}"#,
                r#"[{"type":"stdout"}]"#,
                false,
                10,
                Some(&route.id),
            )
            .unwrap();
        assert_eq!(updated.id, route.id);
        assert!(!updated.enabled);
        assert_eq!(updated.priority, 10);

        assert!(db.delete_event_route(&route.id).unwrap());
        assert!(db.list_event_routes("acc-1").unwrap().is_empty());
    }
}
