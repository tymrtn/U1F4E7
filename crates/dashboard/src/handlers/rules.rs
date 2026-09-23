// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Rules visibility + safe dry-run endpoints for the dashboard.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use envelope_email_store::{Database, Message, Rule};
use envelope_email_transport::rule_exec::{
    self, ActionAttribution, ActionSource, ExecDb, RuleMailbox, RunAccount,
};
use envelope_email_transport::rules::{self, Action, MatchExpr, MessageContext};
use serde::Deserialize;
use serde_json::json;

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
    rule_exec::sanitized_stored_action(action).to_string()
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

    let rules_evaluated = rules_to_check.len();
    let (evaluable, skipped_rules) = rule_exec::split_evaluable_rules(rules_to_check);
    let mut matches = Vec::new();
    for (rule, expr) in &evaluable {
        if rules::evaluate(expr, &ctx) {
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
        "rules_evaluated": rules_evaluated,
        "matches": matches,
        "skipped_rules": skipped_rules,
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
    if match_expr.has_empty_condition_list() {
        return WriteError::new(
            StatusCode::BAD_REQUEST,
            "empty_match_condition",
            rules::EMPTY_CONDITION_LIST_SKIP_REASON,
        )
        .into_response();
    }

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
            let ctx = match rule_exec::build_summary_context(summary, &db, &account_id) {
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

    let no_enabled_rules = {
        let db = state.db.lock().await;
        match db.list_enabled_rules(&account_id) {
            Ok(rules) => rules.is_empty(),
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("rules: {e}")).into_response();
            }
        }
    };

    if no_enabled_rules {
        return Json(json!({
            "processed": 0,
            "actions": 0,
            "log": [],
            "message": "no enabled rules",
        }))
        .into_response();
    }

    let (client_arc, creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response(),
    };

    // Lock order is client -> db (the same order resolve_canonical_folder
    // uses); the executor takes the db guard only inside each with_db call.
    let mut client = client_arc.lock().await;
    let summaries =
        match envelope_email_transport::imap::fetch_inbox(&mut client, &folder, req.limit).await {
            Ok(msgs) => msgs,
            Err(e) => {
                drop(client);
                state.evict_imap(&account_id).await;
                return (StatusCode::BAD_GATEWAY, format!("fetch: {e}")).into_response();
            }
        };

    // Evaluate from the header-only summaries fetched above — the same unified
    // executor as `envelope rule run`: no per-UID full RFC822 fetch/parse.
    let mut mbox = DashboardMailbox {
        state: &state,
        client: &mut client,
        account_id: &account_id,
    };
    let report = rule_exec::apply_rules_to_summaries(
        &mut mbox,
        &DashboardDb(&state),
        &RunAccount {
            id: &account_id,
            email: &creds.account.username,
        },
        &folder,
        &summaries,
        &ActionAttribution::new(ActionSource::Reader),
    )
    .await;

    match report {
        Ok(report) => Json(json!({
            "processed": report.processed,
            "actions": report.actions,
            "log": report.log,
            "skipped_rules": report.skipped_rules,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("rules run: {e:#}"),
        )
            .into_response(),
    }
}

/// [`ExecDb`] over the dashboard's shared handle: the guard is held only for
/// the synchronous closure, never across an IMAP await.
pub(crate) struct DashboardDb<'a>(pub(crate) &'a AppState);

impl ExecDb for DashboardDb<'_> {
    async fn with_db<R>(&self, f: impl FnOnce(&Database) -> R) -> R {
        let db = self.0.db.lock().await;
        f(&db)
    }
}

/// [`RuleMailbox`] over the dashboard's pooled client. Canonical sentinels
/// resolve through `resolve_canonical_folder`, which keeps the dashboard's
/// no-guard-across-await discipline.
pub(crate) struct DashboardMailbox<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) client: &'a mut envelope_email_transport::ImapClient,
    pub(crate) account_id: &'a str,
}

impl envelope_email_transport::threat::persist::RawFetch for DashboardMailbox<'_> {
    async fn fetch_raw(&mut self, folder: &str, uid: u32) -> anyhow::Result<Option<Vec<u8>>> {
        envelope_email_transport::imap::fetch_raw_message(self.client, folder, uid)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fetch UID {uid} in {folder} for scanning: {e}"))
    }
}

impl RuleMailbox for DashboardMailbox<'_> {
    async fn resolve_folder(&mut self, dest: &str) -> anyhow::Result<String> {
        let Some(canonical_type) = envelope_email_transport::folders::canonical_move_key(dest)
        else {
            return Ok(dest.to_string());
        };
        super::messages::resolve_canonical_folder(
            self.state,
            self.client,
            self.account_id,
            canonical_type,
        )
        .await
        .map_err(|e| anyhow::anyhow!("failed to resolve move target {dest}: {e}"))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no provider folder for canonical move target {dest}; not moving into a literal {dest}"
            )
        })
    }

    async fn move_message(&mut self, folder: &str, uid: u32, dest: &str) -> anyhow::Result<()> {
        envelope_email_transport::imap::move_message(self.client, uid, folder, dest)
            .await
            .map_err(|e| anyhow::anyhow!("failed to move UID {uid} to {dest}: {e}"))
    }

    async fn set_flag(&mut self, folder: &str, uid: u32, flag: &str) -> anyhow::Result<()> {
        envelope_email_transport::imap::set_flag(self.client, folder, uid, flag)
            .await
            .map_err(|e| anyhow::anyhow!("failed to set flag '{flag}' on UID {uid}: {e}"))
    }

    async fn remove_flag(&mut self, folder: &str, uid: u32, flag: &str) -> anyhow::Result<()> {
        envelope_email_transport::imap::remove_flag(self.client, folder, uid, flag)
            .await
            .map_err(|e| anyhow::anyhow!("failed to remove flag '{flag}' from UID {uid}: {e}"))
    }

    async fn delete_message(&mut self, folder: &str, uid: u32) -> anyhow::Result<()> {
        envelope_email_transport::imap::delete_message(self.client, folder, uid)
            .await
            .map_err(|e| anyhow::anyhow!("failed to delete UID {uid}: {e}"))
    }

    async fn ensure_folder(&mut self, name: &str) -> anyhow::Result<()> {
        envelope_email_transport::imap::create_folder(self.client, name)
            .await
            .map_err(|e| anyhow::anyhow!("failed to create folder {name}: {e}"))
    }

    async fn list_unsubscribe_headers(
        &mut self,
        folder: &str,
        uid: u32,
    ) -> anyhow::Result<(Option<String>, Option<String>)> {
        envelope_email_transport::imap::fetch_list_unsubscribe_headers(self.client, folder, uid)
            .await
            .map_err(|e| anyhow::anyhow!("List-Unsubscribe headers for UID {uid}: {e}"))
    }
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

fn validate_write_request(
    req: &RuleWriteRequest,
    db: &Database,
    account_id: &str,
) -> Result<(String, String), WriteError> {
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
    let match_expr = serde_json::from_str::<MatchExpr>(&match_expr_json).map_err(|e| {
        WriteError::new(
            StatusCode::BAD_REQUEST,
            "invalid_match_expr",
            format!("invalid match expression: {e}"),
        )
    })?;
    if match_expr.has_empty_condition_list() {
        return Err(WriteError::new(
            StatusCode::BAD_REQUEST,
            "empty_match_condition",
            "match_expr has an empty condition list (an \"and\" or \"or\" with no conditions), \
             which can match every message; add at least one condition",
        ));
    }

    // Validate the action; a confirm offer's rule references are flattened
    // into concrete allowlisted actions here, at save time.
    let action: Action =
        rule_exec::flatten_authored_action(db, account_id, &req.action).map_err(|e| {
            WriteError::new(
                StatusCode::BAD_REQUEST,
                "invalid_action",
                format!("invalid action: {e}"),
            )
        })?;
    let action_json = serde_json::to_string(&action).map_err(|e| {
        WriteError::new(
            StatusCode::BAD_REQUEST,
            "invalid_action",
            format!("invalid action JSON: {e}"),
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
    let db = state.db.lock().await;
    let (match_expr_json, action_json) = match validate_write_request(&req, &db, &account_id) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };

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
    let db = state.db.lock().await;
    let (match_expr_json, action_json) = match validate_write_request(&req, &db, &account_id) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };

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

fn build_message_context(
    msg: &Message,
    db: &Database,
    account_id: &str,
) -> anyhow::Result<MessageContext> {
    // Canonicalize so summary/full/persistence keys agree (IMAP ENVELOPE ids
    // arrive bracketed; persisted scores/tags use the bare id).
    let message_id =
        envelope_email_store::canonical_message_id(msg.message_id.as_deref().unwrap_or(""));

    let tags: Vec<String> = if message_id.is_empty() {
        Vec::new()
    } else {
        db.get_tags(account_id, message_id)?
            .into_iter()
            .map(|t| t.tag)
            .collect()
    };

    let mut scores: HashMap<String, f64> = if message_id.is_empty() {
        HashMap::new()
    } else {
        db.get_scores(account_id, message_id)?
            .into_iter()
            .map(|s| (s.dimension, s.value))
            .collect()
    };
    // Seed the header-derived provider_spam signal; a persisted score wins.
    rules::merge_provider_spam(&mut scores, msg.provider_spam);

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
    use super::{RuleRunRequest, RuleWriteRequest, sanitized_action_json, validate_write_request};

    fn validate(req: &RuleWriteRequest) -> Result<(String, String), super::WriteError> {
        let db = super::Database::open_memory().unwrap();
        validate_write_request(req, &db, "acct")
    }
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
    fn summary_context_seeds_provider_spam_and_uses_canonical_id() {
        use super::{Database, rules};
        use envelope_email_store::MessageSummary;
        use envelope_email_transport::rule_exec::build_summary_context;

        fn summary(uid: u32, message_id: &str, provider_spam: Option<f64>) -> MessageSummary {
            MessageSummary {
                uid,
                message_id: Some(message_id.to_string()),
                from_addr: "s@example.com".to_string(),
                to_addr: "me@example.com".to_string(),
                subject: "x".to_string(),
                date: None,
                flags: vec![],
                size: 10,
                provider_spam,
            }
        }

        let db = Database::open_memory().unwrap();

        // The dashboard run/preview path builds context from the header-only
        // summary — the derived provider_spam seeds the dimension with no
        // full-message parse.
        let derived = summary(1, "<derived@example.com>", Some(6.5));
        let ctx = build_summary_context(&derived, &db, "acct").unwrap();
        assert_eq!(ctx.scores.get(rules::PROVIDER_SPAM_DIMENSION), Some(&6.5));

        // A persisted score keyed on the bare id is found for a bracketed
        // summary id and wins over the derived header value.
        db.set_score(
            "acct",
            "pinned@example.com",
            rules::PROVIDER_SPAM_DIMENSION,
            2.0,
            None,
            None,
        )
        .unwrap();
        let pinned = summary(2, "<pinned@example.com>", Some(9.9));
        let ctx = build_summary_context(&pinned, &db, "acct").unwrap();
        assert_eq!(
            ctx.scores.get(rules::PROVIDER_SPAM_DIMENSION),
            Some(&2.0),
            "dashboard summary evaluation must find the persisted bare-id score and let it win"
        );
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
        let result = validate(&req);
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
        let result = validate(&req);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rule_write_request_rejects_empty_condition_lists() {
        for match_expr in [
            serde_json::json!({ "and": [] }),
            serde_json::json!({ "or": [{ "from": "*@x" }, { "and": [] }] }),
        ] {
            let req = RuleWriteRequest {
                name: "oops".to_string(),
                match_expr: match_expr.clone(),
                action: serde_json::json!("delete"),
                priority: 100,
                stop: false,
                enabled: true,
            };
            let err = validate(&req).unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{match_expr}");
            assert_eq!(err.code, "empty_match_condition", "{match_expr}");
            assert!(err.message.contains("empty condition list"), "{match_expr}");
        }
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
        let result = validate(&req);
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
        let result = validate(&req);
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
        let err = validate(&req).unwrap_err();
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
            let err = validate(&req).unwrap_err();
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
        let (_, action_json) = validate(&req).unwrap();
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
                validate(&req).is_ok(),
                "action {action_json} should be valid"
            );
        }
    }

    /// Records mailbox calls; never opens a socket.
    #[derive(Default)]
    struct FakeMailbox {
        calls: Vec<String>,
    }

    impl super::RuleMailbox for FakeMailbox {
        async fn resolve_folder(&mut self, dest: &str) -> anyhow::Result<String> {
            Ok(dest.to_string())
        }
        async fn move_message(&mut self, folder: &str, uid: u32, dest: &str) -> anyhow::Result<()> {
            self.calls.push(format!("move {folder}/{uid} -> {dest}"));
            Ok(())
        }
        async fn set_flag(&mut self, folder: &str, uid: u32, flag: &str) -> anyhow::Result<()> {
            self.calls.push(format!("flag {folder}/{uid} {flag}"));
            Ok(())
        }
        async fn remove_flag(&mut self, folder: &str, uid: u32, flag: &str) -> anyhow::Result<()> {
            self.calls.push(format!("unflag {folder}/{uid} {flag}"));
            Ok(())
        }
        async fn delete_message(&mut self, folder: &str, uid: u32) -> anyhow::Result<()> {
            self.calls.push(format!("delete {folder}/{uid}"));
            Ok(())
        }
        async fn ensure_folder(&mut self, name: &str) -> anyhow::Result<()> {
            self.calls.push(format!("create {name}"));
            Ok(())
        }
        async fn list_unsubscribe_headers(
            &mut self,
            _folder: &str,
            _uid: u32,
        ) -> anyhow::Result<(Option<String>, Option<String>)> {
            Ok((None, None))
        }
    }

    #[tokio::test]
    async fn dashboard_run_uses_the_unified_executor_through_the_locked_db() {
        use super::{ActionAttribution, ActionSource, DashboardDb, RunAccount, rule_exec};
        use envelope_email_store::{CredentialBackend, Database, MessageSummary};

        let db = Database::open_memory().unwrap();
        db.create_rule(
            "acct",
            "trips",
            r#"{"from":"*@airline.example"}"#,
            r#"{"add_tag":"travel"}"#,
            10,
            false,
        )
        .unwrap();
        db.create_rule(
            "acct",
            "later",
            r#"{"from":"*@airline.example"}"#,
            r#"{"snooze":"1d"}"#,
            20,
            false,
        )
        .unwrap();
        let state = crate::state::AppState::new(db, CredentialBackend::File);
        let summaries = [MessageSummary {
            uid: 11,
            message_id: Some("<trip@airline.example>".to_string()),
            from_addr: "desk@airline.example".to_string(),
            to_addr: "me@example.com".to_string(),
            subject: "Itinerary".to_string(),
            date: None,
            flags: vec![],
            size: 1,
            provider_spam: None,
        }];
        let mut mbox = FakeMailbox::default();

        let report = rule_exec::apply_rules_to_summaries(
            &mut mbox,
            &DashboardDb(&state),
            &RunAccount {
                id: "acct",
                email: "me@example.com",
            },
            "INBOX",
            &summaries,
            &ActionAttribution::new(ActionSource::Reader),
        )
        .await
        .unwrap();

        assert_eq!(report.actions, 1, "{report:?}");
        assert_eq!(report.skipped_rules.len(), 1, "snooze rule is gated");
        assert!(mbox.calls.is_empty());
        let db = state.db.lock().await;
        assert_eq!(
            db.get_tags("acct", "trip@airline.example").unwrap()[0].tag,
            "travel"
        );
        let rows = db.list_actions("acct", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].action_taken.contains("\"source\":\"reader\""));
    }

    #[test]
    fn write_request_flattens_confirm_rule_reference() {
        let db = super::Database::open_memory().unwrap();
        db.create_rule(
            "acct",
            "travel-tag",
            r#"{"from":"*@x.com"}"#,
            r#"{"add_tag":"travel"}"#,
            100,
            false,
        )
        .unwrap();
        let req = RuleWriteRequest {
            name: "offer".to_string(),
            match_expr: serde_json::json!({ "from": "*@x.com" }),
            action: serde_json::json!({"confirm": {"prompt": "Trip?", "then": [{"rule": "travel-tag"}]}}),
            priority: 100,
            stop: false,
            enabled: true,
        };
        let (_, action_json) = validate_write_request(&req, &db, "acct").unwrap();
        assert_eq!(
            action_json,
            r#"{"confirm":{"prompt":"Trip?","then":[{"add_tag":"travel"}]}}"#
        );

        let bad = RuleWriteRequest {
            action: serde_json::json!({"confirm": {"prompt": "p", "then": [{"move": "Trash"}]}}),
            ..req
        };
        let err = validate_write_request(&bad, &db, "acct").unwrap_err();
        assert_eq!(err.code, "invalid_action");
    }
}
