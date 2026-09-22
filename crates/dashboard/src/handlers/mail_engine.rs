// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Read-only local mail-engine cockpit data. This handler never decrypts
//! credentials, probes IMAP, calls OpenRouter, delivers events, or mutates mail.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use crate::ui_paths::message_dashboard_path;

#[derive(Debug, Deserialize)]
pub struct DecisionQuery {
    account_id: Option<String>,
    route: Option<String>,
    status: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct CorrectionRequest {
    folder: String,
    expected_revision: u64,
    route: String,
    urgency: String,
}

pub async fn correct(
    State(state): State<AppState>,
    Path((account, uid)): Path<(String, u32)>,
    Json(request): Json<CorrectionRequest>,
) -> Response {
    let db = state.db.lock().await;
    let account_id = db
        .list_accounts()
        .unwrap_or_default()
        .into_iter()
        .find(|candidate| candidate.id == account || candidate.username == account)
        .map(|candidate| candidate.id)
        .unwrap_or(account);
    match db.correct_current_mail_engine_decision(
        &account_id,
        &request.folder,
        uid,
        request.expected_revision,
        &request.route,
        &request.urgency,
        "dashboard",
    ) {
        Ok(Some(revision)) => Json(json!({
            "ok": true,
            "account_id": account_id,
            "folder": request.folder,
            "uid": uid,
            "revision": revision,
            "effective_route": request.route,
            "effective_urgency": request.urgency,
        }))
        .into_response(),
        Ok(None) => (
            StatusCode::CONFLICT,
            Json(json!({
                "code": "engine_revision_conflict",
                "reason": "the decision changed, is busy, or no longer exists; refresh before correcting"
            })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "code": "engine_invalid_correction",
                "reason": "route or urgency is not part of the stable mail-engine contract"
            })),
        )
            .into_response(),
    }
}

pub async fn decisions(
    State(state): State<AppState>,
    Query(query): Query<DecisionQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "code": "invalid_mail_engine_limit",
                "reason": "limit must be between 1 and 200"
            })),
        )
            .into_response();
    }

    let db = state.db.lock().await;
    let displays = match db.list_mail_engine_decision_display(
        query.account_id.as_deref(),
        query.route.as_deref(),
        query.status.as_deref(),
        limit,
    ) {
        Ok(items) => items,
        Err(error) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "code": "invalid_mail_engine_filter",
                    "reason": error.to_string()
                })),
            )
                .into_response();
        }
    };
    let status_rows = match db.list_mail_engine_status(query.account_id.as_deref()) {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "code": "mail_engine_status_failed",
                    "reason": error.to_string()
                })),
            )
                .into_response();
        }
    };
    let pending_digest = db
        .count_pending_mail_engine_digest(query.account_id.as_deref())
        .unwrap_or(0);
    let matching_notification_routes = db
        .list_accounts()
        .unwrap_or_default()
        .into_iter()
        .filter(|account| {
            query
                .account_id
                .as_deref()
                .is_none_or(|selected| selected == account.id || selected == account.username)
        })
        .flat_map(|account| db.list_event_routes(&account.id).unwrap_or_default())
        .filter(|route| route.enabled && route_matches_urgent(&route.match_expr))
        .count();

    let items = displays
        .into_iter()
        .map(|item| {
            let decision = item.decision;
            json!({
                "account_id": decision.account_id,
                "folder": decision.folder,
                "uidvalidity": decision.uidvalidity,
                "uid": decision.uid,
                "backend": decision.backend,
                "model": decision.model,
                "model_status": decision.model_status,
                "status": decision.status,
                "model_route": decision.route,
                "route": decision.effective_route,
                "route_probability": decision.route_probability,
                "route_confidence": decision.route_confidence,
                "model_urgency": decision.urgency,
                "urgency": decision.effective_urgency,
                "correction_revision": decision.correction_revision,
                "notify_user_probability": decision.notify_user_probability,
                "requires_reply_probability": decision.requires_reply_probability,
                "bulk_or_subscription_probability": decision.bulk_or_subscription_probability,
                "execution_status": decision.execution_status,
                "executed_action": decision.executed_action,
                "model_error_code": decision.model_error_code,
                "error_code": decision.error_code,
                "decided_at": decision.decided_at,
                "message_link": message_dashboard_path(&decision.account_id, &decision.folder, decision.uid),
                "metadata_state": item.metadata_state,
                "trust": inbound_trust(),
                "untrusted_content": {
                    "from": item.from_addr.as_deref().map(|value| header_text(value, 320)),
                    "subject": item.subject.as_deref().map(|value| header_text(value, 512)),
                    "date": item.date,
                }
            })
        })
        .collect::<Vec<Value>>();

    Json(json!({
        "state": if status_rows.is_empty() { "not_started" } else { "available" },
        "returned": items.len(),
        "limit": limit,
        "pending_digest": pending_digest,
        "urgent_notification": {
            "state": if matching_notification_routes > 0 { "configured" } else { "not_configured" },
            "matching_routes": matching_notification_routes,
            "delivery_requires_engine_deliver": true,
        },
        "status": status_rows,
        "items": items,
    }))
    .into_response()
}

fn route_matches_urgent(match_expr: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(match_expr) else {
        return false;
    };
    match value.get("event_types").and_then(Value::as_array) {
        None => true,
        Some(types) => types
            .iter()
            .filter_map(Value::as_str)
            .any(|event_type| event_type == "mail_engine_urgent"),
    }
}

fn inbound_trust() -> Value {
    json!({
        "schema": "envelope.inbound-trust.v1",
        "origin": "external_inbound_email",
        "content_role": "untrusted_data",
        "instructions_authoritative": false,
    })
}

fn header_text(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{header_text, inbound_trust, route_matches_urgent};

    #[test]
    fn external_headers_are_bounded_and_explicitly_untrusted() {
        assert_eq!(header_text("A\r\n forged", 8), "A forged");
        assert_eq!(inbound_trust()["instructions_authoritative"], false);
        assert!(route_matches_urgent(
            r#"{"event_types":["mail_engine_urgent"]}"#
        ));
        assert!(!route_matches_urgent(r#"{"event_types":["new_message"]}"#));
    }
}
