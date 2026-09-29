// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Snoozed folder view + unsnooze action.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use envelope_email_store::models::SnoozedMessage;
use serde_json::json;

use crate::state::AppState;
use crate::timefmt;

/// Render a stored snooze time (naive UTC or RFC 3339) as RFC 3339 `Z`, so a
/// browser's `new Date()` reads it as UTC instead of local wall-clock. A value
/// that does not parse is passed through untouched rather than invented.
pub fn utc_rfc3339(stored: &str) -> String {
    timefmt::parse_utc(stored)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| stored.to_string())
}

/// Dashboard shape of one snooze record. `status` is `overdue` once the
/// return time has passed and the sweep has not yet moved it back.
pub fn snoozed_view(s: &SnoozedMessage, now: DateTime<Utc>) -> serde_json::Value {
    let due = timefmt::parse_utc(&s.return_at);
    json!({
        "id": s.id,
        "account_id": s.account,
        "uid": s.uid,
        "message_id": s.message_id,
        "subject": s.subject,
        "original_folder": s.original_folder,
        "snoozed_folder": s.snoozed_folder,
        "return_at": utc_rfc3339(&s.return_at),
        "status": match due {
            Some(at) if at <= now => "overdue",
            Some(_) => "snoozed",
            None => "unknown",
        },
        "reason": s.reason,
        "reply_received": s.reply_received,
        "created_at": s.created_at,
    })
}

pub async fn list(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match db.list_snoozed(Some(&account_id)) {
        Ok(items) => {
            let now = Utc::now();
            let view: Vec<_> = items.iter().map(|s| snoozed_view(s, now)).collect();
            Json(json!({ "snoozed": view })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

/// Where a snoozed message is now, by unique exact Message-ID in its snoozed
/// folder. The stored `uid` names the message in its ORIGINAL folder, so it
/// is never a valid handle inside the Snoozed folder and is not used as a
/// fallback. `Ok(None)` means no exact, unique match.
pub async fn locate_snoozed(
    client: &mut envelope_email_transport::ImapClient,
    snoozed: &SnoozedMessage,
) -> Result<Option<u32>, envelope_email_transport::errors::ImapError> {
    let Some(mid) = snoozed.message_id.as_deref() else {
        return Ok(None);
    };
    // FETCH-based: a SEARCH index can lag a just-completed snooze move.
    envelope_email_transport::imap::find_unique_uid_by_message_id_from(
        client,
        &snoozed.snoozed_folder,
        mid,
        1,
    )
    .await
}

pub async fn unsnooze(
    State(state): State<AppState>,
    Path((account_id, snoozed_id)): Path<(String, String)>,
) -> impl IntoResponse {
    // Look up the snooze record
    let snoozed = {
        let db = state.db.lock().await;
        match db.get_snoozed(&snoozed_id) {
            // A record owned by another account is not addressable from this
            // path: answer exactly as if it did not exist.
            Ok(Some(s)) if s.account == account_id => s,
            Ok(_) => return (StatusCode::NOT_FOUND, "snooze record not found").into_response(),
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}"))
                    .into_response();
            }
        }
    };
    if snoozed.message_id.is_none() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "code": "snooze_target_unknown",
                "reason": "this snooze has no Message-ID, so the message cannot be found exactly in the Snoozed folder; move it from there by hand",
            })),
        )
            .into_response();
    }

    // Connect IMAP and move the message back
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    let current_uid = match locate_snoozed(&mut client, &snoozed).await {
        Ok(Some(uid)) => uid,
        Ok(None) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "code": "snoozed_message_not_found",
                    "reason": format!(
                        "no single message with this Message-ID is in {}; it may already have returned",
                        snoozed.snoozed_folder
                    ),
                })),
            )
                .into_response();
        }
        Err(e) => {
            state.evict_imap(&account_id).await;
            return (StatusCode::BAD_GATEWAY, format!("find snoozed: {e}")).into_response();
        }
    };

    if let Err(e) = envelope_email_transport::imap::move_message(
        &mut client,
        current_uid,
        &snoozed.snoozed_folder,
        &snoozed.original_folder,
    )
    .await
    {
        state.evict_imap(&account_id).await;
        return (StatusCode::BAD_GATEWAY, format!("move back: {e}")).into_response();
    }

    drop(client);
    // The message is back. A failed record delete leaves a row the sweep will
    // later skip (no exact match in Snoozed); report it rather than hide it.
    let record_cleared = {
        let db = state.db.lock().await;
        db.delete_snoozed(&snoozed.id)
    };

    state
        .events
        .publish(crate::events::DashboardEvent::Unsnoozed {
            account_id: snoozed.account.clone(),
            original_folder: snoozed.original_folder.clone(),
        });

    Json(json!({
        "ok": true,
        "id": snoozed_id,
        "moved_to": snoozed.original_folder,
        "record_cleared": record_cleared.is_ok(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(return_at: &str) -> SnoozedMessage {
        SnoozedMessage {
            id: "s1".into(),
            account: "acc1".into(),
            uid: 42,
            original_folder: "INBOX".into(),
            snoozed_folder: "Snoozed".into(),
            return_at: return_at.into(),
            message_id: Some("m@x".into()),
            subject: Some("Hi".into()),
            created_at: "2026-09-01T00:00:00".into(),
            reason: Some("dashboard".into()),
            note: None,
            recipient: None,
            escalation_tier: 0,
            reply_received: false,
        }
    }

    #[test]
    fn stored_naive_utc_is_served_with_a_z_suffix() {
        assert_eq!(utc_rfc3339("2026-11-01T13:00:00"), "2026-11-01T13:00:00Z");
        assert_eq!(
            utc_rfc3339("2026-11-01T14:00:00+01:00"),
            "2026-11-01T13:00:00Z"
        );
        assert_eq!(utc_rfc3339("garbage"), "garbage");
    }

    #[test]
    fn view_reports_overdue_after_the_return_time() {
        let now = timefmt::parse_utc("2026-11-01T13:00:01Z").unwrap();
        let overdue = snoozed_view(&record("2026-11-01T13:00:00"), now);
        assert_eq!(overdue["status"], "overdue");
        assert_eq!(overdue["account_id"], "acc1");
        assert_eq!(overdue["return_at"], "2026-11-01T13:00:00Z");
        let pending = snoozed_view(&record("2026-11-01T13:00:02"), now);
        assert_eq!(pending["status"], "snoozed");
    }
}
