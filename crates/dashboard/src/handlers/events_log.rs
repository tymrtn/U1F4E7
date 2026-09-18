// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `GET /api/events` — the Logs page: what agents and Envelope did, newest
//! first, read from the `events` table. Read-only. Each entry is a fixed
//! projection — never the raw payload, the message snippet, or a subject hash;
//! the draft subject comes from the draft row itself, which the operator can
//! already open.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use envelope_email_store::{Account, Database, EventLogEntry, EventLogFilter};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use crate::ui_paths::{draft_dashboard_path, message_dashboard_path};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
/// Governor attributes shown per entry; the draft page has the full picture.
const MAX_ATTRS: usize = 8;

#[derive(Deserialize)]
pub struct LogsQuery {
    pub account: Option<String>,
    #[serde(rename = "type")]
    pub event_type: Option<String>,
    pub since: Option<String>,
    pub before: Option<String>,
    pub limit: Option<usize>,
}

fn invalid(message: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": message, "code": "events_query_invalid" })),
    )
        .into_response()
}

pub async fn get(State(state): State<AppState>, Query(q): Query<LogsQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return invalid("limit must be between 1 and 500");
    }
    for (name, value) in [("since", &q.since), ("before", &q.before)] {
        if let Some(value) = value
            && crate::timefmt::parse_utc(value).is_none()
        {
            return invalid(&format!("{name} must be an RFC 3339 timestamp"));
        }
    }
    if let Some(t) = &q.event_type
        && (t.is_empty()
            || !t
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.'))
    {
        return invalid("type must be an event type or a dotted prefix of one");
    }

    let db = state.db.lock().await;
    let accounts = match db.list_accounts() {
        Ok(accounts) => accounts,
        Err(e) => return store_error(e),
    };
    let filter = EventLogFilter {
        account_id: q.account.clone(),
        event_type: q.event_type.clone(),
        since: q.since.clone(),
        before: q.before.clone(),
        limit,
    };
    let entries = match db.list_event_log(&filter) {
        Ok(entries) => entries,
        Err(e) => return store_error(e),
    };
    // A full page may have more behind it; a short page is the end. The cursor
    // is the newest row of the last entry, so the next page can return the
    // older tail of that same-day run as a continuation the client merges by
    // `group_key` — nothing between two entries is ever skipped.
    let next_before = (entries.len() == limit)
        .then(|| entries.last().map(|e| e.event.created_at.clone()))
        .flatten();
    let projected: Vec<Value> = entries
        .iter()
        .map(|entry| project(entry, &accounts, &db))
        .collect();
    Json(json!({
        "entries": projected,
        "next_before": next_before,
        "limit": limit,
    }))
    .into_response()
}

fn store_error(e: envelope_email_store::StoreError) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": format!("{e}"), "code": "store_error" })),
    )
        .into_response()
}

fn project(entry: &EventLogEntry, accounts: &[Account], db: &Database) -> Value {
    let event = &entry.event;
    let payload: Value = event
        .payload
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or(Value::Null);
    let outcome = &payload["outcome"];
    let surface = payload["request"]["surface"]
        .as_str()
        .or_else(|| payload["surface"].as_str());
    let attrs: Vec<Value> = outcome["attribution"]["governor_attrs"]
        .as_array()
        .map(|attrs| attrs.iter().take(MAX_ATTRS).cloned().collect())
        .unwrap_or_default();
    let draft_subject = entry
        .draft_id
        .as_deref()
        .and_then(|id| db.get_draft(id).ok().flatten())
        .and_then(|draft| draft.subject);
    let day = event.created_at.get(..10).unwrap_or(&event.created_at);
    let group_key = format!(
        "{}|{}|{}|{}",
        event.account_id,
        event.event_type,
        entry
            .draft_id
            .as_deref()
            .or(event.message_id.as_deref())
            .unwrap_or(&event.id),
        day
    );
    json!({
        "id": event.id,
        "account_id": event.account_id,
        "account_label": super::review::account_label(accounts, &event.account_id),
        "event_type": event.event_type,
        "created_at": event.created_at,
        "first_at": entry.first_at,
        "repeat_count": entry.repeat_count,
        "group_key": group_key,
        "agent_id": entry.agent_id,
        "acked": event.acked_at.is_some(),
        "draft_id": entry.draft_id,
        "draft_link": entry
            .draft_id
            .as_deref()
            .map(|id| draft_dashboard_path(&event.account_id, id)),
        "draft_subject": draft_subject,
        "surface": surface,
        "decision": outcome["decision"].as_str(),
        "block_code": outcome["block_code"].as_str(),
        "block_reason": outcome["block_reason"].as_str(),
        "attrs": attrs,
        "policy_mode": payload["mode"].as_str(),
        "denial_code": payload["denial_code"].as_str(),
        "message_id": event.message_id,
        "folder": event.folder,
        "uid": event.uid,
        "message_link": event
            .uid
            .map(|uid| message_dashboard_path(&event.account_id, &event.folder, uid)),
        "from_addr": event.from_addr,
        "subject": event.subject,
    })
}
