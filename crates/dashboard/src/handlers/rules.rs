// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Rules visibility + safe dry-run endpoints for the dashboard.

use std::collections::HashMap;

use anyhow::Context;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use envelope_email_store::{Database, Message, Rule};
use envelope_email_transport::rules::{self, Action, MessageContext};
use serde::Deserialize;
use serde_json::json;
use tracing::info;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct RuleTestQuery {
    #[serde(default = "default_folder")]
    pub folder: String,
}

fn default_folder() -> String {
    "INBOX".to_string()
}

fn default_run_limit() -> u32 {
    50
}

fn sanitized_action_json(action: &str) -> String {
    match serde_json::from_str::<Action>(action) {
        Ok(Action::Webhook(_)) => serde_json::to_string(&Action::Webhook("[redacted]".to_string()))
            .unwrap_or_else(|_| "{\"webhook\":\"[redacted]\"}".to_string()),
        Ok(parsed) => {
            serde_json::to_string(&parsed).unwrap_or_else(|_| "\"[invalid action]\"".to_string())
        }
        Err(_) => "\"[invalid action]\"".to_string(),
    }
}

fn dashboard_rule_json(rule: &Rule) -> serde_json::Value {
    json!({
        "id": rule.id,
        "account_id": rule.account_id,
        "name": rule.name,
        "match_expr": rule.match_expr,
        "action": sanitized_action_json(&rule.action),
        "enabled": rule.enabled,
        "priority": rule.priority,
        "stop": rule.stop,
        "sieve_exportable": rule.sieve_exportable,
        "hit_count": rule.hit_count,
        "last_hit_at": rule.last_hit_at,
        "created_at": rule.created_at,
        "updated_at": rule.updated_at,
    })
}

pub async fn list(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match db.list_rules(&account_id) {
        Ok(rules) => {
            let rules: Vec<_> = rules.iter().map(dashboard_rule_json).collect();
            Json(json!({ "rules": rules })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("rules: {e}")).into_response(),
    }
}

pub async fn test_message(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Query(q): Query<RuleTestQuery>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response(),
    };

    let msg = {
        let mut client = client_arc.lock().await;
        match envelope_email_transport::imap::fetch_message(&mut client, &q.folder, uid).await {
            Ok(Some(msg)) => msg,
            Ok(None) => return (StatusCode::NOT_FOUND, "message not found").into_response(),
            Err(e) => {
                state.evict_imap(&account_id).await;
                return (StatusCode::BAD_GATEWAY, format!("fetch: {e}")).into_response();
            }
        }
    };

    let (rules_to_check, ctx) = {
        let db = state.db.lock().await;
        let rules_to_check = match db.list_enabled_rules(&account_id) {
            Ok(rules) => rules,
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("rules: {e}")).into_response();
            }
        };
        let ctx = match build_message_context(&msg, &db, &account_id) {
            Ok(ctx) => ctx,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("rule context: {e}"),
                )
                    .into_response();
            }
        };
        (rules_to_check, ctx)
    };

    let mut matches = Vec::new();
    for rule in &rules_to_check {
        let expr: rules::MatchExpr = match serde_json::from_str(&rule.match_expr) {
            Ok(expr) => expr,
            Err(e) => {
                matches.push(json!({
                    "rule_id": rule.id,
                    "rule_name": rule.name,
                    "priority": rule.priority,
                    "status": "error",
                    "error": format!("invalid match expression: {e}"),
                }));
                continue;
            }
        };

        if rules::evaluate(&expr, &ctx) {
            matches.push(json!({
                "rule_id": rule.id,
                "rule_name": rule.name,
                "priority": rule.priority,
                "action": sanitized_action_json(&rule.action),
                "stop": rule.stop,
                "status": "matched",
            }));
            if rule.stop {
                break;
            }
        }
    }

    Json(json!({
        "uid": uid,
        "folder": q.folder,
        "subject": msg.subject,
        "from": msg.from_addr,
        "rules_evaluated": rules_to_check.len(),
        "matches": matches,
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleRunRequest {
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default = "default_run_limit")]
    pub limit: u32,
}

/// Batch apply enabled rules to messages in a folder (mutating).
///
/// This mirrors `envelope rule run --folder <folder> --limit <n> --json`, but
/// runs inside the dashboard server (no shelling out) and never returns raw
/// credentials.
pub async fn run_enabled(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Json(req): Json<RuleRunRequest>,
) -> impl IntoResponse {
    let folder = req.folder.trim().to_string();
    if folder.is_empty() {
        return (StatusCode::BAD_REQUEST, "folder is required").into_response();
    }
    if !(1..=200).contains(&req.limit) {
        return (StatusCode::BAD_REQUEST, "limit must be between 1 and 200").into_response();
    }

    let enabled_rules = {
        let db = state.db.lock().await;
        match db.list_enabled_rules(&account_id) {
            Ok(rules) => rules,
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("rules: {e}")).into_response();
            }
        }
    };

    if enabled_rules.is_empty() {
        return Json(json!({
            "processed": 0,
            "actions": 0,
            "log": [],
            "message": "no enabled rules",
        }))
        .into_response();
    }

    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response(),
    };

    let summaries = {
        let mut client = client_arc.lock().await;
        match envelope_email_transport::imap::fetch_inbox(&mut client, &folder, req.limit).await {
            Ok(msgs) => msgs,
            Err(e) => {
                state.evict_imap(&account_id).await;
                return (StatusCode::BAD_GATEWAY, format!("fetch: {e}")).into_response();
            }
        }
    };

    let total = summaries.len();
    let uids: Vec<u32> = summaries.iter().map(|s| s.uid).collect();
    let mut actions_taken = 0u32;
    let mut action_log: Vec<serde_json::Value> = Vec::new();

    for &uid in &uids {
        // Fetch full message for evaluation.
        let msg = {
            let mut client = client_arc.lock().await;
            match envelope_email_transport::imap::fetch_message(&mut client, &folder, uid).await {
                Ok(Some(m)) => m,
                Ok(None) => continue, // moved/deleted earlier in the run
                Err(_) => continue,   // treat as transient per-message failure
            }
        };

        // Build message context from local tags/scores/contacts.
        let ctx = {
            let db = state.db.lock().await;
            match build_message_context(&msg, &db, &account_id) {
                Ok(ctx) => ctx,
                Err(_) => continue,
            }
        };

        for rule in &enabled_rules {
            let match_expr: rules::MatchExpr = match serde_json::from_str(&rule.match_expr) {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !rules::evaluate(&match_expr, &ctx) {
                continue;
            }

            let action: Action = match serde_json::from_str(&rule.action) {
                Ok(a) => a,
                Err(_) => continue,
            };

            let action_result = {
                let mut client = client_arc.lock().await;
                execute_action(
                    &mut client,
                    &action,
                    uid,
                    &folder,
                    Some(&rule.name),
                    Some(&ctx),
                )
                .await
            };

            match &action_result {
                Ok(desc) => {
                    info!("rule '{}' fired on UID {uid}: {desc}", rule.name);
                    actions_taken += 1;
                    {
                        let db = state.db.lock().await;
                        let _ = db.increment_rule_hit(&rule.id);
                    }
                    action_log.push(json!({
                        "uid": uid,
                        "rule": rule.name,
                        "action": desc,
                        "status": "ok",
                    }));
                }
                Err(e) => {
                    action_log.push(json!({
                        "uid": uid,
                        "rule": rule.name,
                        "error": format!("{e}"),
                        "status": "error",
                    }));
                }
            }

            if matches!(action, Action::Move(_) | Action::Delete) || rule.stop {
                break;
            }
        }
    }

    Json(json!({
        "processed": total,
        "actions": actions_taken,
        "log": action_log,
    }))
    .into_response()
}

async fn execute_action(
    client: &mut envelope_email_transport::ImapClient,
    action: &Action,
    uid: u32,
    folder: &str,
    rule_name: Option<&str>,
    ctx: Option<&MessageContext>,
) -> anyhow::Result<String> {
    match action {
        Action::Move(dest) => {
            envelope_email_transport::imap::move_message(client, uid, folder, dest)
                .await
                .with_context(|| format!("failed to move UID {uid} to {dest}"))?;
            Ok(format!("moved to {dest}"))
        }
        Action::Flag(flag) => {
            envelope_email_transport::imap::set_flag(client, folder, uid, flag)
                .await
                .with_context(|| format!("failed to set flag '{flag}' on UID {uid}"))?;
            Ok(format!("flagged {flag}"))
        }
        Action::Unflag(flag) => {
            envelope_email_transport::imap::remove_flag(client, folder, uid, flag)
                .await
                .with_context(|| format!("failed to remove flag '{flag}' from UID {uid}"))?;
            Ok(format!("unflagged {flag}"))
        }
        Action::Delete => {
            envelope_email_transport::imap::delete_message(client, folder, uid)
                .await
                .with_context(|| format!("failed to delete UID {uid}"))?;
            Ok("deleted".to_string())
        }
        Action::AddTag(tag) => Ok(format!("add_tag:{tag} (metadata-only, skipped in batch)")),
        Action::Snooze(until) => Ok(format!(
            "snooze:{until} (use 'envelope snooze set' instead)"
        )),
        Action::Unsubscribe => Ok("unsubscribe (use 'envelope unsubscribe' instead)".to_string()),
        Action::Webhook(url) => {
            let payload = serde_json::json!({
                "event": "rule_matched",
                "rule": rule_name.unwrap_or("unknown"),
                "uid": uid,
                "folder": folder,
                "message": {
                    "from": ctx.map(|c| c.from_addr.as_str()).unwrap_or(""),
                    "to": ctx.map(|c| c.to_addr.as_str()).unwrap_or(""),
                    "subject": ctx.map(|c| c.subject.as_str()).unwrap_or(""),
                }
            });
            let http = reqwest::Client::new();
            let body = serde_json::to_vec(&payload)
                .map_err(|e| anyhow::anyhow!("failed to serialize webhook payload: {e}"))?;
            match http
                .post(url.as_str())
                .header("Content-Type", "application/json")
                .body(body)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) => Ok(format!("webhook delivered: {}", resp.status())),
                Err(_) => Err(anyhow::anyhow!("webhook delivery failed")),
            }
        }
    }
}

fn build_message_context(
    msg: &Message,
    db: &Database,
    account_id: &str,
) -> anyhow::Result<MessageContext> {
    let message_id = msg.message_id.as_deref().unwrap_or("");

    let tags: Vec<String> = if message_id.is_empty() {
        Vec::new()
    } else {
        db.get_tags(account_id, message_id)?
            .into_iter()
            .map(|t| t.tag)
            .collect()
    };

    let scores: HashMap<String, f64> = if message_id.is_empty() {
        HashMap::new()
    } else {
        db.get_scores(account_id, message_id)?
            .into_iter()
            .map(|s| (s.dimension, s.value))
            .collect()
    };

    let contact_tags = db.get_contact_tags(account_id, &msg.from_addr)?;

    Ok(MessageContext {
        from_addr: msg.from_addr.clone(),
        to_addr: msg.to_addr.clone(),
        subject: msg.subject.clone(),
        tags,
        scores,
        contact_tags,
    })
}

#[cfg(test)]
mod tests {
    use super::sanitized_action_json;

    #[test]
    fn sanitized_action_json_redacts_webhook_urls() {
        let action = r#"{"webhook":"https://example.com/hook?token=secret#frag"}"#;
        let sanitized = sanitized_action_json(action);
        assert_eq!(sanitized, r#"{"webhook":"[redacted]"}"#);
        assert!(!sanitized.contains("secret"));
        assert!(!sanitized.contains("example.com"));
    }

    #[test]
    fn sanitized_action_json_preserves_non_secret_actions() {
        assert_eq!(
            sanitized_action_json(r#"{"move":"Junk"}"#),
            r#"{"move":"Junk"}"#
        );
        assert_eq!(sanitized_action_json(r#""delete""#), r#""delete""#);
    }
}
