// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Scheduled-send visibility for the v2 Agent Cockpit.
//!
//! `GET /api/scheduled` (aggregate) and `GET /api/accounts/{id}/scheduled`
//! (scoped) list queued drafts carrying a `send_after`, with:
//!   * the ISO 8601 send time and a `due` flag (send_after already passed)
//!   * the latest Governor verdict for that draft (allow/review/block), parsed
//!     from the sanitized `send_governor.*` audit events — content-free
//!   * the outbox cooldown window in effect
//!
//! Read-only: this endpoint never sends, cancels, or mutates a draft. The
//! background scheduled-send sweep (in `crate::serve`) is the only path that
//! transmits queued mail, and it runs the Governor gate itself. Cancelling a
//! scheduled draft is done through the existing per-account draft discard
//! endpoint (`POST /api/accounts/{id}/drafts/{id}/discard`).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use envelope_email_store::{Database, Draft, GovernorVerdict, errors::Result as StoreResult};
use envelope_email_transport::outbound::resolve_cooldown_seconds;
use serde_json::{Value, json};
use std::collections::HashMap;

pub async fn get(State(state): State<AppState>) -> impl IntoResponse {
    respond(&state, None).await
}

pub async fn get_for_account(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    respond(&state, Some(account_id)).await
}

async fn respond(state: &AppState, account_id: Option<String>) -> axum::response::Response {
    // Queued `send_after` values and the sweep's SQLite due-selection are UTC;
    // `now` must be too, or due flags and countdowns skew by the UTC offset.
    let now = crate::timefmt::utc_now_string();
    let db = state.db.lock().await;
    // Resolve a username/id alias to the canonical account id when scoped.
    let resolved = match account_id.as_deref() {
        Some(id) => match resolve_account_id(&db, id) {
            Some(canonical) => Some(canonical),
            None => {
                return Json(json!({
                    "account_status": "not_found",
                    "scheduled": [],
                    "summary": { "scheduled": 0, "due": 0 },
                    "generated_at": now,
                }))
                .into_response();
            }
        },
        None => None,
    };
    match build_scheduled_json(&db, resolved.as_deref(), &now) {
        Ok(payload) => Json(payload).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("scheduled db error: {e}"),
        )
            .into_response(),
    }
}

use crate::state::AppState;

fn resolve_account_id(db: &Database, id: &str) -> Option<String> {
    db.list_accounts().ok()?.into_iter().find_map(|acct| {
        if acct.id == id || acct.username == id {
            Some(acct.id)
        } else {
            None
        }
    })
}

fn build_scheduled_json(db: &Database, account_id: Option<&str>, now: &str) -> StoreResult<Value> {
    let drafts = db.list_scheduled_drafts(account_id, 100)?;
    let verdicts = db.latest_governor_verdicts(200)?;
    let cooldown_seconds = resolve_cooldown_seconds(None);

    let items: Vec<Value> = drafts
        .iter()
        .map(|draft| scheduled_item(draft, &verdicts, now, cooldown_seconds))
        .collect();
    let due = items
        .iter()
        .filter(|item| item.get("due").and_then(Value::as_bool) == Some(true))
        .count();

    Ok(json!({
        "account_status": if account_id.is_some() { "selected" } else { "all" },
        "scheduled": items,
        "summary": { "scheduled": items.len(), "due": due, "cooldown_seconds": cooldown_seconds },
        "generated_at": now,
    }))
}

fn scheduled_item(
    draft: &Draft,
    verdicts: &HashMap<String, GovernorVerdict>,
    now: &str,
    cooldown_seconds: i64,
) -> Value {
    let send_after = draft.send_after.as_deref();
    let now_utc = crate::timefmt::parse_utc(now);
    let due = matches!(
        (send_after.and_then(crate::timefmt::parse_utc), now_utc),
        (Some(sa), Some(n)) if sa <= n
    );
    let seconds_remaining = send_after.and_then(|sa| seconds_between(now, sa));

    let verdict = verdicts.get(&draft.id).map(|v| {
        json!({
            "decision": v.decision,
            "allowed": v.allowed,
            "block_code": v.block_code,
            "verdict": verdict_bucket(v),
            "at": v.created_at,
        })
    });

    json!({
        "id": draft.id,
        "account_id": draft.account_id,
        "subject": draft.subject,
        "created_by": draft.created_by,
        "send_after": draft.send_after,
        "due": due,
        "seconds_remaining": seconds_remaining,
        "cooldown_seconds": cooldown_seconds,
        "governor": verdict,
        "action_base": format!("/api/accounts/{}/drafts/{}", draft.account_id, draft.id),
    })
}

/// Map a Governor decision to the cockpit's three visible buckets so the UI can
/// pick a Badge variant: allow→ok, review→pending, deny/block→danger.
fn verdict_bucket(verdict: &GovernorVerdict) -> &'static str {
    if verdict.allowed {
        return "allow";
    }
    match verdict.decision.as_str() {
        "review" => "review",
        _ => "block",
    }
}

/// Whole seconds from `now` until `future`. Negative or unparsable returns
/// `None` (already due / unknown). Both values are UTC — `now` is RFC 3339 `Z`
/// and `future` may be a legacy naive-UTC row; [`crate::timefmt::parse_utc`]
/// puts them in the same frame.
fn seconds_between(now: &str, future: &str) -> Option<i64> {
    let (n, f) = (
        crate::timefmt::parse_utc(now)?,
        crate::timefmt::parse_utc(future)?,
    );
    let delta = (f - n).num_seconds();
    if delta > 0 { Some(delta) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_account(db: &Database) {
        db.conn().execute("INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password) VALUES ('acc1', 'Test', 'op@example.com', 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'x')", []).unwrap();
    }

    fn insert_governor_event(
        db: &Database,
        id: &str,
        draft_id: &str,
        decision: &str,
        allowed: bool,
    ) {
        let payload = serde_json::json!({
            "request": {"surface": "scheduled", "draft_id": draft_id},
            "outcome": {"allowed": allowed, "decision": decision, "block_code": if allowed { serde_json::Value::Null } else { serde_json::json!("governor_blocked") }},
        });
        db.insert_event(&envelope_email_store::Event {
            id: id.to_string(),
            account_id: "acc1".to_string(),
            event_type: if allowed {
                "send_governor.allowed".into()
            } else {
                "send_governor.blocked".into()
            },
            folder: "policy".to_string(),
            uid: None,
            message_id: None,
            from_addr: None,
            subject: None,
            snippet: None,
            payload: Some(payload.to_string()),
            idempotency_key: None,
            secure_pending: false,
            acked_at: Some("2026-07-02T00:00:00".to_string()),
            created_at: "2026-07-02T00:00:00".to_string(),
        })
        .unwrap();
    }

    #[test]
    fn scheduled_surfaces_governor_verdict_and_due_flag() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        let draft = db
            .create_draft(
                "acc1",
                "to@example.com",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00")
            .unwrap();
        insert_governor_event(&db, "g1", &draft.id, "review", false);

        let payload = build_scheduled_json(&db, None, "2026-07-08T09:00:00").unwrap();
        assert_eq!(payload["summary"]["scheduled"], 1);
        assert_eq!(payload["summary"]["due"], 1);
        assert_eq!(payload["scheduled"][0]["due"], true);
        assert_eq!(payload["scheduled"][0]["governor"]["decision"], "review");
        assert_eq!(payload["scheduled"][0]["governor"]["verdict"], "review");
        assert_eq!(payload["scheduled"][0]["governor"]["allowed"], false);
        // Content-free: no recipient address in the payload.
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains("to@example.com"));
    }

    #[test]
    fn scheduled_countdown_reports_seconds_remaining_for_future_send() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        let draft = db
            .create_draft(
                "acc1",
                "to@example.com",
                Some("Later"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2026-07-08T09:01:00")
            .unwrap();

        let payload = build_scheduled_json(&db, None, "2026-07-08T09:00:00").unwrap();
        assert_eq!(payload["scheduled"][0]["due"], false);
        assert_eq!(payload["scheduled"][0]["seconds_remaining"], 60);
    }

    /// Regression for the UTC-vs-local skew: a draft queued ~120s ahead in the
    /// canonical UTC frame (a legacy naive-UTC row, as the queue writers
    /// produced) must report ~120 seconds against the production `now`. Under
    /// the old `chrono::Local` naive `now` this reported the host's UTC offset
    /// in the countdown (or flipped `due`) on any non-UTC host, and the old
    /// naive-only parser could not read the RFC 3339 `Z` now at all.
    #[test]
    fn scheduled_countdown_is_utc_framed_not_local() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        let draft = db
            .create_draft(
                "acc1",
                "to@example.com",
                Some("Soon"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        let send_after = (chrono::Utc::now() + chrono::Duration::seconds(120))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        db.update_draft_send_after(&draft.id, &send_after).unwrap();

        let now = crate::timefmt::utc_now_string();
        let payload = build_scheduled_json(&db, None, &now).unwrap();
        assert_eq!(payload["scheduled"][0]["due"], false);
        let secs = payload["scheduled"][0]["seconds_remaining"]
            .as_i64()
            .expect("a 120s queued item must report a countdown");
        assert!(
            (110..=120).contains(&secs),
            "expected ~120 seconds remaining, got {secs} (UTC-vs-local skew?)"
        );
        assert_eq!(payload["generated_at"], now);
    }

    /// Mixed representations share one frame: an RFC 3339 `Z` `now` against a
    /// legacy naive-UTC `send_after` compares as the same instant.
    #[test]
    fn due_and_countdown_handle_mixed_naive_and_rfc3339_utc() {
        assert_eq!(
            seconds_between("2026-07-08T09:00:00Z", "2026-07-08T09:02:00"),
            Some(120)
        );
        // Equal instants across representations: due, no countdown.
        assert_eq!(
            seconds_between("2026-07-08T09:00:00Z", "2026-07-08T09:00:00"),
            None
        );

        let db = Database::open_memory().unwrap();
        seed_account(&db);
        let draft = db
            .create_draft(
                "acc1",
                "to@example.com",
                Some("Boundary"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2026-07-08T09:00:00")
            .unwrap();
        let payload = build_scheduled_json(&db, None, "2026-07-08T09:00:00Z").unwrap();
        assert_eq!(payload["scheduled"][0]["due"], true);
        assert_eq!(payload["summary"]["due"], 1);
    }

    /// Structural guard for the UTC-vs-local skew: the surfaces that compare
    /// against stored UTC rows (`send_after`, snooze `return_at`) must never
    /// read local wall-clock time. `crate::timefmt` is the only "now" source.
    #[test]
    fn dashboard_time_surfaces_never_read_local_wall_clock() {
        // Build the needle by concatenation so this test's own source never
        // matches it.
        let needle = format!("Local::{}", "now");
        for (name, source) in [
            ("scheduled.rs", include_str!("scheduled.rs")),
            ("cockpit.rs", include_str!("cockpit.rs")),
            ("drafts.rs", include_str!("drafts.rs")),
            ("compose.rs", include_str!("compose.rs")),
            ("lib.rs", include_str!("../lib.rs")),
        ] {
            assert!(
                !source.contains(&needle),
                "{name} must use crate::timefmt, not chrono::{needle} — \
                 local time skews UTC due comparisons by the host offset"
            );
        }
    }
}
