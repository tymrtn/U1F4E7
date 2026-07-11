// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Rules visibility + safe dry-run endpoints for the dashboard.

use std::collections::HashMap;

use anyhow::Context;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use envelope_email_store::{Database, Message, MessageSummary, Rule};
use envelope_email_transport::rules::{self, Action, MatchExpr, MessageContext};
use serde::Deserialize;
use serde_json::json;
use tracing::info;

use crate::state::AppState;
use crate::ui_paths::message_dashboard_path;

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

pub(crate) fn sanitized_action_json(action: &str) -> String {
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
pub struct RulePreviewRequest {
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default = "default_run_limit")]
    pub limit: u32,
}

/// Non-mutating blast-radius preview for one rule.
pub async fn preview(
    State(state): State<AppState>,
    Path((account_id, rule_id)): Path<(String, String)>,
    Json(req): Json<RulePreviewRequest>,
) -> impl IntoResponse {
    let folder = req.folder.trim().to_string();
    if folder.is_empty() {
        return (StatusCode::BAD_REQUEST, "folder is required").into_response();
    }
    if !(1..=1000).contains(&req.limit) {
        return (StatusCode::BAD_REQUEST, "limit must be between 1 and 1000").into_response();
    }

    let rule = {
        let db = state.db.lock().await;
        match db.get_rule(&rule_id) {
            Ok(Some(rule)) if rule.account_id == account_id => rule,
            Ok(Some(_)) | Ok(None) => {
                return (StatusCode::NOT_FOUND, "rule not found").into_response();
            }
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("rules: {e}")).into_response();
            }
        }
    };
    let match_expr: rules::MatchExpr = match serde_json::from_str(&rule.match_expr) {
        Ok(expr) => expr,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid match expression: {e}"),
            )
                .into_response();
        }
    };

    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response(),
    };

    let summaries = {
        let mut client = client_arc.lock().await;
        match envelope_email_transport::imap::fetch_folder_summaries_read_only(
            &mut client,
            &folder,
            req.limit,
        )
        .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                state.evict_imap(&account_id).await;
                return (StatusCode::BAD_GATEWAY, format!("preview fetch: {e}")).into_response();
            }
        }
    };

    let mut matched = 0u32;
    let mut unread_matched = 0u32;
    let mut samples = Vec::new();
    {
        let db = state.db.lock().await;
        for summary in &summaries {
            let ctx = match build_summary_context(summary, &db, &account_id) {
                Ok(ctx) => ctx,
                Err(_) => continue,
            };
            if !rules::evaluate(&match_expr, &ctx) {
                continue;
            }
            matched += 1;
            let unread = !summary
                .flags
                .iter()
                .any(|flag| flag.to_lowercase().contains("seen"));
            if unread {
                unread_matched += 1;
            }
            if samples.len() < 5 {
                samples.push(json!({
                    "uid": summary.uid,
                    "from": summary.from_addr,
                    "subject": summary.subject,
                    "date": summary.date,
                    "unread": unread,
                    "message_link": message_dashboard_path(&account_id, &folder, summary.uid),
                }));
            }
        }
    }

    Json(json!({
        "rule_id": rule.id,
        "rule_name": rule.name,
        "account_id": account_id,
        "folder": folder,
        "limit": req.limit,
        "processed": summaries.len(),
        "matched": matched,
        "unread_matched": unread_matched,
        "mutated": false,
        "action": sanitized_action_json(&rule.action),
        "enabled": rule.enabled,
        "samples": samples,
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
    #[serde(default)]
    pub confirm: bool,
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
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            "rules run mutates the mailbox; preview/review first and send confirm=true",
        )
            .into_response();
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
                        let _ = db.record_rule_run(envelope_email_store::RuleRunAuditInput {
                            account_id: &account_id,
                            rule_id: Some(&rule.id),
                            rule_name: Some(&rule.name),
                            uid: Some(uid as i64),
                            folder: Some(&folder),
                            action: Some(desc),
                            status: "ok",
                            error: None,
                        });
                    }
                    action_log.push(json!({
                        "uid": uid,
                        "rule": rule.name,
                        "action": desc,
                        "status": "ok",
                    }));
                }
                Err(e) => {
                    {
                        let db = state.db.lock().await;
                        let err = format!("{e}");
                        let _ = db.record_rule_run(envelope_email_store::RuleRunAuditInput {
                            account_id: &account_id,
                            rule_id: Some(&rule.id),
                            rule_name: Some(&rule.name),
                            uid: Some(uid as i64),
                            folder: Some(&folder),
                            action: None,
                            status: "error",
                            error: Some(&err),
                        });
                    }
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

// ── Write endpoints ────────────────────────────────────────────────────────

/// Shared body for create and update — carries the rule definition fields.
///
/// `match_expr` must be valid JSON that deserialises into a `MatchExpr`.
/// `action` must be valid JSON that deserialises into an `Action`. Both are
/// validated server-side before the store write so malformed JSON is rejected
/// with 400 rather than persisted as dead weight.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleWriteRequest {
    pub name: String,
    pub match_expr: serde_json::Value,
    pub action: serde_json::Value,
    #[serde(default = "default_priority")]
    pub priority: i64,
    #[serde(default)]
    pub stop: bool,
    /// Only respected during create. Enable/disable after creation uses the
    /// dedicated endpoints so the intent is always explicit.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_priority() -> i64 {
    100
}

fn default_enabled() -> bool {
    true
}

/// A rejected write request, carrying an HTTP status, a stable machine-readable
/// `code`, and a human-readable message. Rendered as a JSON body so clients can
/// branch on `code` without string-matching the message.
#[derive(Debug)]
pub(crate) struct WriteError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl WriteError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        WriteError {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for WriteError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(json!({ "code": self.code, "error": self.message })),
        )
            .into_response()
    }
}

fn validate_write_request(req: &RuleWriteRequest) -> Result<(String, String), WriteError> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(WriteError::new(
            StatusCode::BAD_REQUEST,
            "name_required",
            "name is required",
        ));
    }
    if name.len() > 200 {
        return Err(WriteError::new(
            StatusCode::BAD_REQUEST,
            "name_too_long",
            "name too long (max 200 chars)",
        ));
    }

    // Validate match_expr round-trips through MatchExpr.
    let match_expr_json = serde_json::to_string(&req.match_expr).map_err(|e| {
        WriteError::new(
            StatusCode::BAD_REQUEST,
            "invalid_match_expr",
            format!("invalid match_expr JSON: {e}"),
        )
    })?;
    if let Err(e) = serde_json::from_str::<MatchExpr>(&match_expr_json) {
        return Err(WriteError::new(
            StatusCode::BAD_REQUEST,
            "invalid_match_expr",
            format!("invalid match expression: {e}"),
        ));
    }

    // Validate action round-trips through Action.
    let action_json = serde_json::to_string(&req.action).map_err(|e| {
        WriteError::new(
            StatusCode::BAD_REQUEST,
            "invalid_action",
            format!("invalid action JSON: {e}"),
        )
    })?;
    let action: Action = serde_json::from_str(&action_json).map_err(|e| {
        WriteError::new(
            StatusCode::BAD_REQUEST,
            "invalid_action",
            format!("invalid action: {e}"),
        )
    })?;

    // SSRF guard: a webhook action's URL must resolve to a public target.
    // Rejects loopback, link-local (incl. cloud metadata), private (RFC 1918),
    // documentation, and non-http(s) schemes before the rule is persisted.
    if let Action::Webhook(url) = &action {
        if let Err(e) = envelope_email_transport::url_guard::check_public_url(url) {
            return Err(WriteError::new(
                StatusCode::BAD_REQUEST,
                "webhook_url_rejected",
                e.to_string(),
            ));
        }
    }

    Ok((match_expr_json, action_json))
}

/// POST /api/accounts/{id}/rules — create a new rule.
pub async fn create(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Json(req): Json<RuleWriteRequest>,
) -> impl IntoResponse {
    let (match_expr_json, action_json) = match validate_write_request(&req) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };

    let db = state.db.lock().await;

    // Duplicate-name guard (mirrors CLI behaviour).
    match db.find_rule_by_name(&account_id, req.name.trim()) {
        Ok(Some(_)) => {
            return (
                StatusCode::CONFLICT,
                format!(
                    "a rule named '{}' already exists for this account",
                    req.name.trim()
                ),
            )
                .into_response();
        }
        Ok(None) => {}
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
    }

    match db.create_rule_with_enabled(
        &account_id,
        req.name.trim(),
        &match_expr_json,
        &action_json,
        req.priority,
        req.stop,
        req.enabled,
    ) {
        Ok(rule) => Json(json!({ "rule": dashboard_rule_json(&rule) })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create rule: {e}"),
        )
            .into_response(),
    }
}

/// PUT /api/accounts/{id}/rules/{rule_id} — update an existing rule's definition.
///
/// Does not alter the `enabled` flag; use the enable/disable endpoints for
/// that so the intent is explicit and auditable.
pub async fn update(
    State(state): State<AppState>,
    Path((account_id, rule_id)): Path<(String, String)>,
    Json(req): Json<RuleWriteRequest>,
) -> impl IntoResponse {
    let (match_expr_json, action_json) = match validate_write_request(&req) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };

    let db = state.db.lock().await;

    match db.update_rule(
        &rule_id,
        &account_id,
        req.name.trim(),
        &match_expr_json,
        &action_json,
        req.priority,
        req.stop,
    ) {
        Ok(Some(rule)) => Json(json!({ "rule": dashboard_rule_json(&rule) })).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "rule not found").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("update rule: {e}"),
        )
            .into_response(),
    }
}

/// DELETE /api/accounts/{id}/rules/{rule_id} — delete a rule permanently.
pub async fn destroy(
    State(state): State<AppState>,
    Path((account_id, rule_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().await;

    // Scope-check: verify the rule belongs to this account before deleting.
    match db.get_rule(&rule_id) {
        Ok(Some(rule)) if rule.account_id != account_id => {
            return (StatusCode::NOT_FOUND, "rule not found").into_response();
        }
        Ok(None) => return (StatusCode::NOT_FOUND, "rule not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
        Ok(Some(_)) => {}
    }

    match db.delete_rule(&rule_id) {
        Ok(true) => Json(json!({ "deleted": rule_id })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "rule not found").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("delete rule: {e}"),
        )
            .into_response(),
    }
}

/// POST /api/accounts/{id}/rules/{rule_id}/enable — enable a disabled rule.
pub async fn enable(
    State(state): State<AppState>,
    Path((account_id, rule_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().await;

    // Scope-check.
    match db.get_rule(&rule_id) {
        Ok(Some(rule)) if rule.account_id != account_id => {
            return (StatusCode::NOT_FOUND, "rule not found").into_response();
        }
        Ok(None) => return (StatusCode::NOT_FOUND, "rule not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
        Ok(Some(_)) => {}
    }

    match db.enable_rule(&rule_id) {
        Ok(true) => Json(json!({ "enabled": true, "rule_id": rule_id })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "rule not found").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("enable rule: {e}"),
        )
            .into_response(),
    }
}

/// POST /api/accounts/{id}/rules/{rule_id}/disable — disable a rule without deleting it.
pub async fn disable(
    State(state): State<AppState>,
    Path((account_id, rule_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().await;

    // Scope-check.
    match db.get_rule(&rule_id) {
        Ok(Some(rule)) if rule.account_id != account_id => {
            return (StatusCode::NOT_FOUND, "rule not found").into_response();
        }
        Ok(None) => return (StatusCode::NOT_FOUND, "rule not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
        Ok(Some(_)) => {}
    }

    match db.disable_rule(&rule_id) {
        Ok(true) => Json(json!({ "enabled": false, "rule_id": rule_id })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "rule not found").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("disable rule: {e}"),
        )
            .into_response(),
    }
}

async fn execute_action(
    client: &mut envelope_email_transport::ImapClient,
    action: &Action,
    uid: u32,
    folder: &str,
    rule_name: Option<&str>,
    ctx: Option<&MessageContext>,
) -> anyhow::Result<String> {
    if let Some(skip) = action.local_execution_skip_reason() {
        return Ok(format!("skipped: {skip}"));
    }
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
        // Server-side Sieve actions are intercepted at the top of this
        // function — these arms are unreachable but kept exhaustive.
        Action::Reject(_) | Action::Ereject(_) => {
            Ok(format!("skipped: {}", rules::SERVER_SIDE_ONLY_SKIP_REASON))
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

fn build_summary_context(
    summary: &MessageSummary,
    db: &Database,
    account_id: &str,
) -> anyhow::Result<MessageContext> {
    let message_id = summary.message_id.as_deref().unwrap_or("");

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

    let contact_tags = db.get_contact_tags(account_id, &summary.from_addr)?;

    Ok(MessageContext {
        from_addr: summary.from_addr.clone(),
        to_addr: summary.to_addr.clone(),
        subject: summary.subject.clone(),
        tags,
        scores,
        contact_tags,
    })
}

#[cfg(test)]
mod tests {
    use super::{RuleRunRequest, RuleWriteRequest, sanitized_action_json, validate_write_request};
    use axum::http::StatusCode;

    #[test]
    fn rule_run_request_requires_explicit_confirmation_by_default() {
        let req: RuleRunRequest = serde_json::from_value(serde_json::json!({
            "folder": "INBOX",
            "limit": 25,
        }))
        .unwrap();
        assert!(!req.confirm);

        let confirmed: RuleRunRequest = serde_json::from_value(serde_json::json!({
            "folder": "INBOX",
            "limit": 25,
            "confirm": true,
        }))
        .unwrap();
        assert!(confirmed.confirm);
    }

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

    #[test]
    fn rule_write_request_rejects_empty_name() {
        let req = RuleWriteRequest {
            name: "  ".to_string(),
            match_expr: serde_json::json!({ "from": "*@x.com" }),
            action: serde_json::json!({ "move": "Archive" }),
            priority: 100,
            stop: false,
            enabled: true,
        };
        let result = validate_write_request(&req);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("name"));
    }

    #[test]
    fn rule_write_request_rejects_invalid_match_expr() {
        let req = RuleWriteRequest {
            name: "test".to_string(),
            // "unknown_field" is not a valid MatchExpr variant.
            match_expr: serde_json::json!({ "unknown_field": "value" }),
            action: serde_json::json!({ "move": "Archive" }),
            priority: 100,
            stop: false,
            enabled: true,
        };
        let result = validate_write_request(&req);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rule_write_request_rejects_invalid_action() {
        let req = RuleWriteRequest {
            name: "test".to_string(),
            match_expr: serde_json::json!({ "from": "*@x.com" }),
            // "explode" is not a valid Action variant.
            action: serde_json::json!({ "explode": "everything" }),
            priority: 100,
            stop: false,
            enabled: true,
        };
        let result = validate_write_request(&req);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rule_write_request_accepts_valid_from_move_rule() {
        let req = RuleWriteRequest {
            name: "GitHub noise".to_string(),
            match_expr: serde_json::json!({ "from": "*@notifications.github.com" }),
            action: serde_json::json!({ "move": "Archive" }),
            priority: 50,
            stop: true,
            enabled: false,
        };
        let result = validate_write_request(&req);
        assert!(result.is_ok());
        let (match_json, action_json) = result.unwrap();
        assert!(match_json.contains("notifications.github.com"));
        assert!(action_json.contains("Archive"));
    }

    #[test]
    fn rule_write_request_rejects_link_local_webhook() {
        // Cloud instance-metadata SSRF target — create path must reject it.
        let req = RuleWriteRequest {
            name: "exfil".to_string(),
            match_expr: serde_json::json!({ "from": "*@x.com" }),
            action: serde_json::json!({ "webhook": "http://169.254.169.254/latest/meta-data/" }),
            priority: 100,
            stop: false,
            enabled: true,
        };
        let err = validate_write_request(&req).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.code, "webhook_url_rejected");
    }

    #[test]
    fn rule_write_request_rejects_private_webhook() {
        // Same validate_write_request feeds both create and update, so this
        // covers the update path too.
        for url in [
            "http://127.0.0.1/hook",
            "http://10.0.0.1/hook",
            "http://localhost:8080/hook",
            "http://[::1]/hook",
        ] {
            let req = RuleWriteRequest {
                name: "exfil".to_string(),
                match_expr: serde_json::json!({ "from": "*@x.com" }),
                action: serde_json::json!({ "webhook": url }),
                priority: 100,
                stop: false,
                enabled: true,
            };
            let err = validate_write_request(&req).unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "url {url}");
            assert_eq!(err.code, "webhook_url_rejected", "url {url}");
        }
    }

    #[test]
    fn rule_write_request_accepts_public_https_webhook() {
        let req = RuleWriteRequest {
            name: "notify".to_string(),
            match_expr: serde_json::json!({ "from": "*@x.com" }),
            action: serde_json::json!({ "webhook": "https://hooks.example.com/envelope" }),
            priority: 100,
            stop: false,
            enabled: true,
        };
        let (_, action_json) = validate_write_request(&req).unwrap();
        assert!(action_json.contains("hooks.example.com"));
    }

    #[test]
    fn rule_write_request_accepts_delete_and_unsubscribe_actions() {
        for action_json in [r#""delete""#, r#""unsubscribe""#] {
            let req = RuleWriteRequest {
                name: "test".to_string(),
                match_expr: serde_json::json!({ "from": "*@x.com" }),
                action: serde_json::from_str(action_json).unwrap(),
                priority: 100,
                stop: false,
                enabled: true,
            };
            assert!(
                validate_write_request(&req).is_ok(),
                "action {action_json} should be valid"
            );
        }
    }
}
