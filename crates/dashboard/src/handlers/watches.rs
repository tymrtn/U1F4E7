// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Watch + delivery health browser for the v2 Agent Cockpit.
//!
//! `GET /api/watches` aggregates, read-only:
//!   * the watch registry (per account+folder run status, heartbeats)
//!   * event routes with delivery counts (delivered / pending / dead), exposing
//!     only a short PREFIX of each route's signing secret — never the full key
//!   * the install-wide dead-letter count
//!
//! Read-only. Nothing here starts, stops, or mutates a watch or route.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use envelope_email_store::{Database, errors::Result as StoreResult};
use serde_json::{Value, json};

use crate::state::AppState;

pub async fn get(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.lock().await;
    match build_watches_json(&db) {
        Ok(payload) => Json(payload).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("watches db error: {e}"),
        )
            .into_response(),
    }
}

fn build_watches_json(db: &Database) -> StoreResult<Value> {
    let accounts = db.list_accounts()?;
    let watches = db.list_watches(None, 50)?;
    let watch_items: Vec<Value> = watches
        .iter()
        .map(|w| {
            json!({
                "id": w.id,
                "account_id": w.account_id,
                "folder": w.folder,
                "status": w.status,
                "schedule": w.schedule,
                "last_heartbeat_at": w.last_heartbeat_at,
                "last_event_at": w.last_event_at,
                "failure_reason": w.failure_reason,
                "health": watch_health(&w.status),
            })
        })
        .collect();

    // Event routes across all accounts, each with delivery health. Only a short
    // prefix of the signing secret is exposed so operators can recognize a route
    // in their vault without the endpoint ever leaking the full key.
    let mut route_items: Vec<Value> = Vec::new();
    for account in &accounts {
        for route in db.list_event_routes(&account.id)? {
            let (delivered, pending, dead) = db.route_delivery_counts(&route.id)?;
            route_items.push(json!({
                "id": route.id,
                "account_id": route.account_id,
                "match_expr": route.match_expr,
                "enabled": route.enabled,
                "priority": route.priority,
                "secret_prefix": route.secret.as_deref().map(secret_prefix),
                "deliveries": { "delivered": delivered, "pending": pending, "dead": dead },
                "health": route_health(dead, route.enabled),
                "created_at": route.created_at,
                "updated_at": route.updated_at,
            }));
        }
    }

    let dead_letter_count = db.dead_letter_count()?;

    Ok(json!({
        "watches": watch_items,
        "routes": route_items,
        "summary": {
            "watches": watch_items.len(),
            "routes": route_items.len(),
            "dead_letter": dead_letter_count,
        },
    }))
}

/// First 10 chars of a route secret (`evrt_` + 5 hex) — enough to recognize,
/// never enough to forge a signature.
fn secret_prefix(secret: &str) -> String {
    secret.chars().take(10).collect()
}

pub(crate) fn watch_health(status: &str) -> &'static str {
    match status {
        "running" => "ok",
        "failed" | "error" => "danger",
        _ => "pending",
    }
}

fn route_health(dead: i64, enabled: bool) -> &'static str {
    if dead > 0 {
        "danger"
    } else if enabled {
        "ok"
    } else {
        "pending"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_account(db: &Database) {
        db.conn().execute("INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password) VALUES ('acc1', 'Test', 'op@example.com', 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'x')", []).unwrap();
    }

    #[test]
    fn watches_exposes_only_secret_prefix_and_delivery_counts() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        db.upsert_watch(envelope_email_store::WatchUpsert {
            account_id: "acc1",
            folder: "INBOX",
            status: "running",
            process_id: Some(1),
            schedule: Some("foreground"),
            last_heartbeat_at: Some("2026-07-08T08:59:00"),
            last_event_at: Some("2026-07-08T08:58:00"),
            failure_reason: None,
        })
        .unwrap();
        let route = db
            .create_event_route(
                "acc1",
                r#"{"event_types":["new_message"]}"#,
                r#"{"type":"webhook","url":"https://example.test/hook"}"#,
                true,
                100,
            )
            .unwrap();
        let full_secret = route.secret.clone().unwrap();
        // One dead-lettered delivery for this route.
        db.enqueue_delivery("d1", "e1", &route.id, "dk1", "2000-01-01T00:00:00")
            .unwrap();
        db.record_delivery_failure(
            "d1",
            Some(500),
            None,
            Some("boom"),
            None,
            "2026-07-08T00:00:00",
        )
        .unwrap();

        let payload = build_watches_json(&db).unwrap();
        assert_eq!(payload["summary"]["watches"], 1);
        assert_eq!(payload["watches"][0]["health"], "ok");
        assert_eq!(payload["summary"]["dead_letter"], 1);
        assert_eq!(payload["routes"][0]["deliveries"]["dead"], 1);
        assert_eq!(payload["routes"][0]["health"], "danger");

        // The full signing secret must never appear; only its short prefix.
        let prefix = payload["routes"][0]["secret_prefix"].as_str().unwrap();
        assert_eq!(prefix.len(), 10);
        assert!(prefix.starts_with("evrt_"));
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(
            !serialized.contains(&full_secret),
            "full route secret leaked"
        );
    }
}
