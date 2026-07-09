// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::EventRoute;
use rusqlite::params;
use uuid::Uuid;

/// Columns selected for every [`EventRoute`] read, in struct-field order.
const ROUTE_COLUMNS: &str = "id, account_id, match_expr, delivery, enabled, priority, \
     secret, created_at, updated_at";

impl Database {
    /// Create a new event route with a freshly generated HMAC signing secret.
    ///
    /// The returned route carries the plaintext `secret`; callers must surface
    /// it to the operator exactly once and never persist it elsewhere. Later
    /// reads still return the secret from the row, so redaction is the caller's
    /// job at the presentation boundary (the CLI `events routes list` never
    /// prints it).
    pub fn create_event_route(
        &self,
        account_id: &str,
        match_expr: &str,
        delivery: &str,
        enabled: bool,
        priority: i64,
    ) -> Result<EventRoute> {
        let id = Uuid::new_v4().to_string();
        let secret = generate_route_secret();
        self.conn().execute(
            "INSERT INTO event_routes (id, account_id, match_expr, delivery, enabled, priority, secret)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, account_id, match_expr, delivery, enabled, priority, secret],
        )?;
        self.get_event_route(&id)
    }

    /// Create or replace an event route. A secret is minted on first insert and
    /// preserved across updates (updating a route never rotates its secret).
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
        let secret = generate_route_secret();
        self.conn().execute(
            "INSERT INTO event_routes (id, account_id, match_expr, delivery, enabled, priority, secret)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                account_id = excluded.account_id,
                match_expr = excluded.match_expr,
                delivery = excluded.delivery,
                enabled = excluded.enabled,
                priority = excluded.priority,
                secret = COALESCE(event_routes.secret, excluded.secret),
                updated_at = datetime('now')",
            params![id, account_id, match_expr, delivery, enabled, priority, secret],
        )?;
        self.get_event_route(&id)
    }

    /// Fetch a single event route by id.
    pub fn get_event_route(&self, route_id: &str) -> Result<EventRoute> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {ROUTE_COLUMNS} FROM event_routes WHERE id = ?1"
        ))?;
        Ok(stmt.query_row(params![route_id], map_event_route)?)
    }

    /// List event routes for an account in priority order.
    pub fn list_event_routes(&self, account_id: &str) -> Result<Vec<EventRoute>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {ROUTE_COLUMNS} FROM event_routes
             WHERE account_id = ?1
             ORDER BY priority ASC, created_at ASC"
        ))?;
        let rows = stmt.query_map(params![account_id], map_event_route)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// List enabled event routes across all accounts, or scoped to one account.
    /// Used by the delivery enqueue path when a watch emits an event.
    pub fn list_enabled_event_routes(&self, account_id: Option<&str>) -> Result<Vec<EventRoute>> {
        let (sql, bind): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = match account_id {
            Some(id) => (
                format!(
                    "SELECT {ROUTE_COLUMNS} FROM event_routes
                     WHERE enabled = 1 AND account_id = ?1
                     ORDER BY priority ASC, created_at ASC"
                ),
                vec![Box::new(id.to_string())],
            ),
            None => (
                format!(
                    "SELECT {ROUTE_COLUMNS} FROM event_routes
                     WHERE enabled = 1
                     ORDER BY priority ASC, created_at ASC"
                ),
                vec![],
            ),
        };
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bind.iter()), map_event_route)?;
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

/// Generate a random route secret. 32 bytes of UUID entropy, hex encoded,
/// prefixed so operators can recognize it in their own vaults.
fn generate_route_secret() -> String {
    let a = Uuid::new_v4().simple().to_string();
    let b = Uuid::new_v4().simple().to_string();
    format!("evrt_{a}{b}")
}

fn map_event_route(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRoute> {
    Ok(EventRoute {
        id: row.get(0)?,
        account_id: row.get(1)?,
        match_expr: row.get(2)?,
        delivery: row.get(3)?,
        enabled: row.get(4)?,
        priority: row.get(5)?,
        secret: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
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

    #[test]
    fn create_event_route_mints_unique_secret() {
        let db = Database::open_memory().unwrap();
        let a = db
            .create_event_route(
                "acc-1",
                r#"{"event_types":["otp_detected"]}"#,
                r#"{"type":"webhook","url":"https://x"}"#,
                true,
                100,
            )
            .unwrap();
        let b = db
            .create_event_route(
                "acc-1",
                r#"{"event_types":["new_message"]}"#,
                r#"{"type":"webhook","url":"https://y"}"#,
                true,
                100,
            )
            .unwrap();

        let sa = a.secret.expect("route a must have a secret");
        let sb = b.secret.expect("route b must have a secret");
        assert!(
            sa.starts_with("evrt_"),
            "secret should carry the evrt_ prefix"
        );
        assert_ne!(sa, sb, "each route gets an independent secret");

        let reloaded = db.get_event_route(&a.id).unwrap();
        assert_eq!(reloaded.secret.as_deref(), Some(sa.as_str()));
    }

    #[test]
    fn upsert_preserves_secret_across_updates() {
        let db = Database::open_memory().unwrap();
        let created = db
            .create_event_route(
                "acc-1",
                "{}",
                r#"{"type":"webhook","url":"https://x"}"#,
                true,
                100,
            )
            .unwrap();
        let secret = created.secret.clone().unwrap();

        let updated = db
            .upsert_event_route(
                "acc-1",
                "{}",
                r#"{"type":"webhook","url":"https://z"}"#,
                false,
                10,
                Some(&created.id),
            )
            .unwrap();
        assert_eq!(
            updated.secret.as_deref(),
            Some(secret.as_str()),
            "updating a route must not rotate its signing secret"
        );
    }

    #[test]
    fn list_enabled_scopes_by_account_and_enabled_flag() {
        let db = Database::open_memory().unwrap();
        db.create_event_route("acc-1", "{}", "{}", true, 100)
            .unwrap();
        db.create_event_route("acc-1", "{}", "{}", false, 100)
            .unwrap();
        db.create_event_route("acc-2", "{}", "{}", true, 100)
            .unwrap();

        assert_eq!(
            db.list_enabled_event_routes(Some("acc-1")).unwrap().len(),
            1
        );
        assert_eq!(db.list_enabled_event_routes(None).unwrap().len(), 2);
    }
}
