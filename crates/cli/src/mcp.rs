// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! MCP (Model Context Protocol) server for Envelope Email.
//!
//! Implements the MCP stdio transport: reads JSON-RPC requests from stdin,
//! dispatches to existing command functions, writes JSON-RPC responses to stdout.

use crate::commands::attachments::{
    attachment_summaries, decode_attachments, snapshot_attachments,
};
use crate::commands::contract::{DEFAULT_AGENT_LIST_LIMIT, MAX_AGENT_LIST_LIMIT};
use crate::commands::drafts::sent_mail_proof_json;
use crate::commands::governor_gate::{account_domain, gate_and_record, governor_request};
use crate::commands::ui;
use envelope_email_store::{CredentialBackend, Database, Event};
use envelope_email_transport::outbound::{
    IMMEDIATE_SEND_CONFIRM_CODE, OUTBOX_COOLDOWN_REASON, OUTBOX_COOLDOWN_REASON_CODE,
    SendDisposition, SendSurface, resolve_cooldown_seconds, resolve_disposition,
};
use envelope_email_transport::{
    SendMode, SendPolicyDecision, SendPolicyInput, SendRuntime, audit_event_for,
    default_mode_for_runtime, evaluate,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::str::FromStr;

// ── JSON-RPC types ──────────────────────────────────────────────────

#[derive(Deserialize)]
#[allow(dead_code)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl JsonRpcResponse {
    fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<Value>, code: i64, message: String) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message,
                data: None,
            }),
        }
    }
}

// ── MCP protocol types ──────────────────────────────────────────────

fn server_info() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "envelope",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

pub(crate) fn tool_list() -> Value {
    crate::commands::contract::mcp_tool_list()
}

/// Validate an MCP `limit` parameter for read-only list/search surfaces.
///
/// Returns the resolved limit as `u32`. Rejects 0 and any value above
/// `MAX_AGENT_LIST_LIMIT` before any IMAP work occurs.
fn validate_agent_list_limit(raw: Option<&Value>) -> Result<u32, String> {
    let value = match raw {
        None => DEFAULT_AGENT_LIST_LIMIT as u64,
        Some(value) => value.as_u64().ok_or_else(|| {
            "limit must be an unsigned integer for agent read-only list/search surfaces".to_string()
        })?,
    };
    if value == 0 {
        return Err("limit must be at least 1".to_string());
    }
    if value > MAX_AGENT_LIST_LIMIT as u64 {
        return Err(format!(
            "limit must be at most {MAX_AGENT_LIST_LIMIT} for agent read-only list/search surfaces"
        ));
    }
    Ok(value as u32)
}

// ── Tool dispatch ───────────────────────────────────────────────────

async fn handle_tool_call(
    tool_name: &str,
    params: &Value,
    backend: CredentialBackend,
) -> Result<Value, String> {
    match tool_name {
        "accounts" => handle_accounts(backend).await,
        "inbox" => handle_inbox(params, backend).await,
        "read" => handle_read(params, backend).await,
        "search" => handle_search(params, backend).await,
        "send" => handle_send(params, backend).await,
        "reply" => handle_reply(params, backend).await,
        "create_reply_draft" => handle_create_reply_draft(params, backend).await,
        "create_forward_draft" => handle_create_forward_draft(params, backend).await,
        "modify_draft" => handle_modify_draft(params, backend).await,
        "get_draft" => handle_get_draft(params, backend).await,
        "send_draft" => handle_send_draft(params, backend).await,
        "move_message" => handle_move(params, backend).await,
        "flag" => handle_flag(params, backend).await,
        "folders" => handle_folders(params, backend).await,
        "tag" => handle_tag(params, backend).await,
        "contacts" => handle_contacts(params, backend).await,
        _ => Err(format!("unknown tool: {tool_name}")),
    }
}

async fn handle_accounts(_backend: CredentialBackend) -> Result<Value, String> {
    let db = Database::open_default().map_err(|e| e.to_string())?;
    let accounts = db.list_accounts().map_err(|e| e.to_string())?;
    Ok(Value::Array(
        accounts
            .iter()
            .map(|account| ui::with_ui(account, ui::account_ui(&account.id)))
            .collect(),
    ))
}

async fn handle_inbox(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let limit = validate_agent_list_limit(params.get("limit"))?;
    let account_arg = params.get("account").and_then(|v| v.as_str());
    let folder = params
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("INBOX");

    let (_db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .map_err(|e| e.to_string())?;

    let messages = envelope_email_transport::imap::fetch_inbox(&mut client, folder, limit)
        .await
        .map_err(|e| e.to_string())?;

    Ok(Value::Array(
        messages
            .iter()
            .map(|message| {
                ui::with_ui(
                    message,
                    ui::message_ui(&creds.account.id, message.uid, folder),
                )
            })
            .collect(),
    ))
}

async fn handle_read(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let uid = params
        .get("uid")
        .and_then(|v| v.as_u64())
        .ok_or("uid is required")? as u32;
    let folder = params
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("INBOX");
    let account_arg = params.get("account").and_then(|v| v.as_str());

    let (_db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .map_err(|e| e.to_string())?;

    let message = envelope_email_transport::imap::fetch_message(&mut client, folder, uid)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("message {uid} not found in {folder}"))?;

    Ok(ui::with_ui(
        &message,
        ui::message_ui(&creds.account.id, message.uid, folder),
    ))
}

async fn handle_search(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let query = params
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or("query is required")?;
    let limit = validate_agent_list_limit(params.get("limit"))?;
    let folder = params
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("INBOX");
    let account_arg = params.get("account").and_then(|v| v.as_str());

    let (_db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .map_err(|e| e.to_string())?;

    let messages = envelope_email_transport::imap::search(&mut client, folder, query, limit)
        .await
        .map_err(|e| e.to_string())?;

    Ok(Value::Array(
        messages
            .iter()
            .map(|message| {
                ui::with_ui(
                    message,
                    ui::message_ui(&creds.account.id, message.uid, folder),
                )
            })
            .collect(),
    ))
}

async fn handle_send(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let to = params
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or("to is required")?;
    let subject = params
        .get("subject")
        .and_then(|v| v.as_str())
        .ok_or("subject is required")?;
    let body = params.get("body").and_then(|v| v.as_str());
    let html = params.get("html").and_then(|v| v.as_str());
    let from = params.get("from").and_then(|v| v.as_str());
    let cc = params.get("cc").and_then(|v| v.as_str());
    let bcc = params.get("bcc").and_then(|v| v.as_str());
    let reply_to = params.get("reply_to").and_then(|v| v.as_str());
    let attach_paths = optional_string_array(params, &["attach", "attachments"])?;
    let account_arg = params.get("account").and_then(|v| v.as_str());
    let send_mode = params
        .get("send_mode")
        .and_then(|v| v.as_str())
        .map(SendMode::from_str)
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| default_mode_for_runtime(SendRuntime::AgentMcp));
    let confirm_send = params
        .get("confirm_send")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let allow_recipients = params
        .get("allow_recipient")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let cooldown_override = params.get("cooldown_seconds").and_then(|v| v.as_i64());
    let send_now = params
        .get("send_now")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let confirm_send_now = params
        .get("confirm_send_now")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let (db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;
    let policy_input = SendPolicyInput {
        to,
        cc,
        bcc,
        confirm_send,
        allow_recipients: &allow_recipients,
    };

    let decision = evaluate(send_mode, &policy_input);
    record_send_policy_event(&db, &creds.account.id, send_mode, &decision, &policy_input);

    match decision {
        SendPolicyDecision::Allowed => {}
        SendPolicyDecision::DraftOnly => {
            let attachment_snapshots =
                snapshot_attachments(&attach_paths).map_err(|e| e.to_string())?;
            let draft = db
                .create_draft(
                    &creds.account.id,
                    to,
                    Some(subject),
                    body,
                    html,
                    None,
                    cc,
                    bcc,
                    Some("mcp"),
                )
                .map_err(|e| e.to_string())?;
            if !attachment_snapshots.is_empty() {
                db.update_draft_attachments(&draft.id, &attachment_snapshots)
                    .map_err(|e| e.to_string())?;
            }
            return Ok(json!({
                "sent": false,
                "status": "drafted",
                "send_mode": send_mode,
                "draft_id": draft.id,
                "attachments": attachment_summaries(&attachment_snapshots),
                "ui": ui::draft_ui(&creds.account.id, &draft.id),
            }));
        }
        SendPolicyDecision::Denied(denial) => {
            return Err(json!({
                "status": "denied",
                "error": denial,
                "send_mode": send_mode,
                "ui": ui::account_ui(&creds.account.id),
            })
            .to_string());
        }
    }

    let attachment_snapshots = snapshot_attachments(&attach_paths).map_err(|e| e.to_string())?;

    // ── Default actual-send cooldown (outbox queueing) ──
    // An allowed MCP send queues by default. Real SMTP only happens later via
    // the scheduled-send sweep, after the Governor gate permits it. Immediate
    // transmission requires an explicit, confirmed bypass.
    let cooldown = resolve_cooldown_seconds(cooldown_override);
    match resolve_disposition(cooldown, send_now, confirm_send_now) {
        SendDisposition::NeedsConfirmation => {
            return Err(json!({
                "status": "denied",
                "error": {
                    "code": IMMEDIATE_SEND_CONFIRM_CODE,
                    "reason": "immediate send bypasses the outbox cooldown; pass send_now=true together with confirm_send_now=true",
                },
            })
            .to_string());
        }
        SendDisposition::Queue {
            cooldown_seconds: cd,
        } => {
            let draft = db
                .create_draft(
                    &creds.account.id,
                    to,
                    Some(subject),
                    body,
                    html,
                    None,
                    cc,
                    bcc,
                    Some("mcp"),
                )
                .map_err(|e| e.to_string())?;
            if !attachment_snapshots.is_empty() {
                db.update_draft_attachments(&draft.id, &attachment_snapshots)
                    .map_err(|e| e.to_string())?;
            }
            let send_at = (chrono::Utc::now() + chrono::Duration::seconds(cd))
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            db.update_draft_send_after(&draft.id, &send_at)
                .map_err(|e| e.to_string())?;
            return Ok(json!({
                "sent": false,
                "status": "queued",
                "send_mode": send_mode,
                "draft_id": draft.id,
                "send_after": send_at,
                "cooldown_seconds": cd,
                "queued_reason_code": OUTBOX_COOLDOWN_REASON_CODE,
                "queued_reason": OUTBOX_COOLDOWN_REASON,
                "attachments": attachment_summaries(&attachment_snapshots),
                "ui": ui::draft_ui(&creds.account.id, &draft.id),
            }));
        }
        SendDisposition::Immediate => {}
    }

    let attachments = decode_attachments(&attachment_snapshots).map_err(|e| e.to_string())?;

    // ── Governor gate (fail-closed before any real SMTP) ──
    let gov_req = governor_request(
        &creds.account.id,
        account_domain(&creds.account.username),
        subject,
        to,
        cc,
        bcc,
        SendSurface::Mcp,
        None,
        &attachments,
        false,
    );
    let gov_outcome = gate_and_record(&db, &creds.account.id, &gov_req);
    if !gov_outcome.allowed {
        return Err(json!({
            "status": "blocked",
            "error": gov_outcome.denial_json(),
        })
        .to_string());
    }

    let message_id = envelope_email_transport::smtp::SmtpSender::send(
        &creds,
        to,
        subject,
        body,
        html,
        from,
        cc,
        bcc,
        reply_to,
        None,
        None,
        &attachments,
    )
    .await
    .map_err(|e| e.to_string())?;

    // Resolve Sent-folder copy using pre-append lookup semantics.
    let from_for_sent = from
        .map(str::to_string)
        .unwrap_or_else(|| crate::commands::drafts::account_from_header(&creds));
    let provider_type = db.get_provider_type(&creds.account.id).ok().flatten();
    let copy_result = crate::commands::drafts::resolve_sent_copy_after_send(
        &db,
        &creds,
        provider_type.as_deref(),
        &from_for_sent,
        to,
        subject,
        body,
        html,
        cc,
        None,
        &[],
        &message_id,
        &attachments,
    )
    .await;

    let sent_mail_appended = copy_result.sent_mail_appended;
    let sent_mail_append_skipped_reason = copy_result.sent_mail_append_skipped_reason;
    let sent_mail_proof = copy_result.proof;
    let provider_sent_copy = if matches!(sent_mail_proof.copy_source, "provider" | "unresolved") {
        Some(sent_mail_proof_json(&creds.account.id, &sent_mail_proof))
    } else {
        None
    };
    let client_appended_copy = if sent_mail_proof.copy_source == "client_appended" {
        Some(sent_mail_proof_json(&creds.account.id, &sent_mail_proof))
    } else {
        None
    };
    let sent_message_url = sent_mail_proof.message_url(&creds.account.id);
    let sent_ui = sent_mail_proof.ui(&creds.account.id);

    Ok(json!({
        "sent": true,
        "message_id": message_id,
        "sent_mail_appended": sent_mail_appended,
        "sent_mail_append_skipped_reason": sent_mail_append_skipped_reason,
        "sent_folder": sent_mail_proof.folder.clone(),
        "sent_uid": sent_mail_proof.uid,
        "sent_message_url": sent_message_url,
        "sent_mail": sent_mail_proof_json(&creds.account.id, &sent_mail_proof),
        "provider_sent_copy": provider_sent_copy,
        "client_appended_copy": client_appended_copy,
        "attachments": attachment_summaries(&attachment_snapshots),
        "ui": sent_ui,
    }))
}

fn record_send_policy_event(
    db: &Database,
    account_id: &str,
    mode: SendMode,
    decision: &SendPolicyDecision,
    input: &SendPolicyInput<'_>,
) {
    let audit = audit_event_for(mode, decision, input);
    let now = chrono::Utc::now().to_rfc3339();
    let event = Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: account_id.to_string(),
        event_type: audit.event.to_string(),
        folder: "policy".to_string(),
        uid: None,
        message_id: None,
        from_addr: None,
        subject: None,
        snippet: None,
        payload: Some(audit.payload.to_string()),
        idempotency_key: None,
        secure_pending: false,
        acked_at: Some(now.clone()),
        created_at: now,
    };
    let _ = db.insert_event(&event);
}

fn required_str<'a>(params: &'a Value, name: &str) -> Result<&'a str, String> {
    params
        .get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{name} is required"))
}

fn optional_str<'a>(params: &'a Value, name: &str) -> Option<&'a str> {
    params.get(name).and_then(|v| v.as_str())
}

fn optional_string_array(params: &Value, names: &[&str]) -> Result<Vec<String>, String> {
    for name in names {
        if let Some(value) = params.get(*name) {
            let Some(items) = value.as_array() else {
                return Err(format!("{name} must be an array of file paths"));
            };
            return items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("{name} entries must be strings"))
                })
                .collect();
        }
    }
    Ok(Vec::new())
}

fn required_uid(params: &Value) -> Result<u32, String> {
    params
        .get("uid")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .ok_or_else(|| "uid is required".to_string())
}

async fn handle_reply(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let uid = params
        .get("uid")
        .and_then(|v| v.as_u64())
        .ok_or("uid is required")? as u32;
    let body = params
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or("body is required")?;
    let html = params.get("html").and_then(|v| v.as_str());
    let reply_all = params
        .get("reply_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let folder = params
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("INBOX");
    let account_arg = params.get("account").and_then(|v| v.as_str());
    let attach_paths = optional_string_array(params, &["attach", "attachments"])?;
    let send_mode = params
        .get("send_mode")
        .and_then(|v| v.as_str())
        .map(SendMode::from_str)
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| default_mode_for_runtime(SendRuntime::AgentMcp));
    let confirm_send = params
        .get("confirm_send")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let allow_recipients = params
        .get("allow_recipient")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let cooldown_override = params.get("cooldown_seconds").and_then(|v| v.as_i64());
    let send_now = params
        .get("send_now")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let confirm_send_now = params
        .get("confirm_send_now")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let (db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .map_err(|e| e.to_string())?;

    let parent = envelope_email_transport::imap::fetch_message(&mut client, folder, uid)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("message {uid} not found in {folder}"))?;

    let headers = if reply_all {
        envelope_email_transport::reply::build_reply_all_headers(&parent, &creds.account.username)
    } else {
        envelope_email_transport::reply::build_reply_headers(&parent)
    };

    let cc_str = if headers.cc.is_empty() {
        None
    } else {
        Some(headers.cc.join(", "))
    };
    let policy_input = SendPolicyInput {
        to: &headers.to,
        cc: cc_str.as_deref(),
        bcc: None,
        confirm_send,
        allow_recipients: &allow_recipients,
    };
    let decision = evaluate(send_mode, &policy_input);
    record_send_policy_event(&db, &creds.account.id, send_mode, &decision, &policy_input);

    match decision {
        SendPolicyDecision::Allowed => {}
        SendPolicyDecision::DraftOnly => {
            let attachment_snapshots =
                snapshot_attachments(&attach_paths).map_err(|e| e.to_string())?;
            let draft = db
                .create_draft(
                    &creds.account.id,
                    &headers.to,
                    Some(&headers.subject),
                    Some(body),
                    html,
                    headers.in_reply_to.as_deref(),
                    cc_str.as_deref(),
                    None,
                    Some("mcp"),
                )
                .map_err(|e| e.to_string())?;
            if !attachment_snapshots.is_empty() {
                db.update_draft_attachments(&draft.id, &attachment_snapshots)
                    .map_err(|e| e.to_string())?;
            }
            db.set_draft_metadata(
                &draft.id,
                &json!({
                    "draft_kind": "reply",
                    "in_reply_to": headers.in_reply_to.clone(),
                    "references": headers.references.clone(),
                    "source": {"folder": folder, "uid": uid},
                }),
            )
            .map_err(|e| e.to_string())?;
            return Ok(json!({
                "sent": false,
                "status": "drafted",
                "send_mode": send_mode,
                "draft_id": draft.id,
                "in_reply_to": headers.in_reply_to,
                "attachments": attachment_summaries(&attachment_snapshots),
                "ui": ui::draft_ui(&creds.account.id, &draft.id),
            }));
        }
        SendPolicyDecision::Denied(denial) => {
            return Err(json!({
                "status": "denied",
                "error": denial,
                "send_mode": send_mode,
                "ui": ui::message_ui(&creds.account.id, uid, folder),
            })
            .to_string());
        }
    }

    let attachment_snapshots = snapshot_attachments(&attach_paths).map_err(|e| e.to_string())?;

    // ── Default actual-send cooldown (outbox queueing) ──
    // An allowed MCP reply queues by default; real SMTP happens later via the
    // scheduled-send sweep, after the Governor gate permits it. Immediate
    // transmission requires an explicit, confirmed bypass.
    let cooldown = resolve_cooldown_seconds(cooldown_override);
    match resolve_disposition(cooldown, send_now, confirm_send_now) {
        SendDisposition::NeedsConfirmation => {
            return Err(json!({
                "status": "denied",
                "error": {
                    "code": IMMEDIATE_SEND_CONFIRM_CODE,
                    "reason": "immediate send bypasses the outbox cooldown; pass send_now=true together with confirm_send_now=true",
                },
            })
            .to_string());
        }
        SendDisposition::Queue {
            cooldown_seconds: cd,
        } => {
            let draft = db
                .create_draft(
                    &creds.account.id,
                    &headers.to,
                    Some(&headers.subject),
                    Some(body),
                    html,
                    headers.in_reply_to.as_deref(),
                    cc_str.as_deref(),
                    None,
                    Some("mcp"),
                )
                .map_err(|e| e.to_string())?;
            if !attachment_snapshots.is_empty() {
                db.update_draft_attachments(&draft.id, &attachment_snapshots)
                    .map_err(|e| e.to_string())?;
            }
            db.set_draft_metadata(
                &draft.id,
                &json!({
                    "draft_kind": "reply",
                    "in_reply_to": headers.in_reply_to.clone(),
                    "references": headers.references.clone(),
                    "source": {"folder": folder, "uid": uid},
                }),
            )
            .map_err(|e| e.to_string())?;
            let send_at = (chrono::Utc::now() + chrono::Duration::seconds(cd))
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            db.update_draft_send_after(&draft.id, &send_at)
                .map_err(|e| e.to_string())?;
            return Ok(json!({
                "sent": false,
                "status": "queued",
                "send_mode": send_mode,
                "draft_id": draft.id,
                "send_after": send_at,
                "cooldown_seconds": cd,
                "queued_reason_code": OUTBOX_COOLDOWN_REASON_CODE,
                "queued_reason": OUTBOX_COOLDOWN_REASON,
                "in_reply_to": headers.in_reply_to,
                "attachments": attachment_summaries(&attachment_snapshots),
                "ui": ui::draft_ui(&creds.account.id, &draft.id),
            }));
        }
        SendDisposition::Immediate => {}
    }

    let attachments = decode_attachments(&attachment_snapshots).map_err(|e| e.to_string())?;

    // ── Governor gate (fail-closed before any real SMTP) ──
    let gov_req = governor_request(
        &creds.account.id,
        account_domain(&creds.account.username),
        &headers.subject,
        &headers.to,
        cc_str.as_deref(),
        None,
        SendSurface::Mcp,
        None,
        &attachments,
        true,
    );
    let gov_outcome = gate_and_record(&db, &creds.account.id, &gov_req);
    if !gov_outcome.allowed {
        return Err(json!({
            "status": "blocked",
            "error": gov_outcome.denial_json(),
        })
        .to_string());
    }

    let message_id = envelope_email_transport::smtp::SmtpSender::send(
        &creds,
        &headers.to,
        &headers.subject,
        Some(body),
        html,
        None,
        cc_str.as_deref(),
        None,
        None,
        headers.in_reply_to.as_deref(),
        Some(&headers.references),
        &attachments,
    )
    .await
    .map_err(|e| e.to_string())?;

    // Resolve Sent-folder copy using pre-append lookup semantics.
    let from_for_sent = crate::commands::drafts::account_from_header(&creds);
    let provider_type = db.get_provider_type(&creds.account.id).ok().flatten();
    let copy_result = crate::commands::drafts::resolve_sent_copy_after_send(
        &db,
        &creds,
        provider_type.as_deref(),
        &from_for_sent,
        &headers.to,
        &headers.subject,
        Some(body),
        html,
        cc_str.as_deref(),
        headers.in_reply_to.as_deref(),
        &headers.references,
        &message_id,
        &attachments,
    )
    .await;

    let sent_mail_appended = copy_result.sent_mail_appended;
    let sent_mail_append_skipped_reason = copy_result.sent_mail_append_skipped_reason;
    let sent_mail_proof = copy_result.proof;
    let provider_sent_copy = if matches!(sent_mail_proof.copy_source, "provider" | "unresolved") {
        Some(sent_mail_proof_json(&creds.account.id, &sent_mail_proof))
    } else {
        None
    };
    let client_appended_copy = if sent_mail_proof.copy_source == "client_appended" {
        Some(sent_mail_proof_json(&creds.account.id, &sent_mail_proof))
    } else {
        None
    };
    let sent_message_url = sent_mail_proof.message_url(&creds.account.id);
    let sent_ui = sent_mail_proof.ui(&creds.account.id);

    Ok(json!({
        "sent": true,
        "message_id": message_id,
        "sent_mail_appended": sent_mail_appended,
        "sent_mail_append_skipped_reason": sent_mail_append_skipped_reason,
        "sent_folder": sent_mail_proof.folder.clone(),
        "sent_uid": sent_mail_proof.uid,
        "sent_message_url": sent_message_url,
        "sent_mail": sent_mail_proof_json(&creds.account.id, &sent_mail_proof),
        "provider_sent_copy": provider_sent_copy,
        "client_appended_copy": client_appended_copy,
        "attachments": attachment_summaries(&attachment_snapshots),
        "in_reply_to": headers.in_reply_to,
        "ui": sent_ui,
        "parent_ui": ui::message_ui(&creds.account.id, uid, folder),
    }))
}

async fn handle_create_reply_draft(
    params: &Value,
    backend: CredentialBackend,
) -> Result<Value, String> {
    let uid = required_uid(params)?;
    let folder = optional_str(params, "folder").unwrap_or("INBOX");
    let account_arg = optional_str(params, "account");
    let reply_all = params
        .get("reply_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let add_signature = params
        .get("add_signature")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let body = optional_str(params, "body");
    let html = optional_str(params, "html");
    let attach_paths = optional_string_array(params, &["attach", "attachments"])?;

    let (db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;
    let draft = crate::commands::drafts::create_reply_draft(
        &db,
        &creds,
        uid,
        folder,
        reply_all,
        body,
        html,
        add_signature,
        &attach_paths,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(crate::commands::drafts::draft_envelope_json(&draft))
}

async fn handle_create_forward_draft(
    params: &Value,
    backend: CredentialBackend,
) -> Result<Value, String> {
    let uid = required_uid(params)?;
    let folder = optional_str(params, "folder").unwrap_or("INBOX");
    let account_arg = optional_str(params, "account");
    let to = optional_str(params, "to");
    let add_signature = params
        .get("add_signature")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let body = optional_str(params, "body");
    let html = optional_str(params, "html");
    let attach_paths = optional_string_array(params, &["attach", "attachments"])?;
    let include_attachments = params
        .get("include_attachments")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let (db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;
    let draft = crate::commands::drafts::create_forward_draft(
        &db,
        &creds,
        uid,
        folder,
        to,
        body,
        html,
        add_signature,
        &attach_paths,
        include_attachments,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(crate::commands::drafts::draft_envelope_json(&draft))
}

async fn handle_modify_draft(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let id = required_str(params, "draft_id")?;
    let account_arg = optional_str(params, "account");
    let add_signature = params.get("add_signature").and_then(|v| v.as_bool());
    let attach_paths = optional_string_array(params, &["attach", "attachments"])?;
    let remove_attachments =
        optional_string_array(params, &["remove_attach", "remove_attachments"])?;
    let clear_attachments = params
        .get("clear_attachments")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let (db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;
    let draft = crate::commands::drafts::modify_draft(
        &db,
        &creds,
        id,
        optional_str(params, "body"),
        optional_str(params, "html"),
        optional_str(params, "to"),
        optional_str(params, "cc"),
        optional_str(params, "bcc"),
        optional_str(params, "subject"),
        add_signature,
        &attach_paths,
        &remove_attachments,
        clear_attachments,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(crate::commands::drafts::draft_envelope_json(&draft))
}

async fn handle_get_draft(params: &Value, _backend: CredentialBackend) -> Result<Value, String> {
    let id = required_str(params, "draft_id")?;
    let db = Database::open_default().map_err(|e| e.to_string())?;
    let draft = db
        .get_draft(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("draft not found: {id}"))?;
    Ok(crate::commands::drafts::draft_envelope_json(&draft))
}

async fn handle_send_draft(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let id = required_str(params, "draft_id")?;
    let account_arg = optional_str(params, "account");
    let confirm_send = params
        .get("confirm_send")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !confirm_send {
        return Ok(json!({
            "status": "denied",
            "draft_id": id,
            "error": {
                "code": "confirm_send_required",
                "reason": "send_draft requires confirm_send=true in MCP agent contexts"
            }
        }));
    }

    let cooldown_override = params.get("cooldown_seconds").and_then(|v| v.as_i64());
    let send_now = params
        .get("send_now")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let confirm_send_now = params
        .get("confirm_send_now")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // ── Default actual-send cooldown (outbox queueing) ──
    // send_draft queues by default: it sets send_after on the draft and leaves
    // it at status=draft (scheduled). Real SMTP only happens later via the
    // scheduled-send sweep, after the Governor gate permits it. Immediate
    // transmission requires an explicit, confirmed bypass.
    let cooldown = resolve_cooldown_seconds(cooldown_override);
    match resolve_disposition(cooldown, send_now, confirm_send_now) {
        SendDisposition::NeedsConfirmation => {
            return Ok(json!({
                "status": "denied",
                "draft_id": id,
                "error": {
                    "code": IMMEDIATE_SEND_CONFIRM_CODE,
                    "reason": "immediate send bypasses the outbox cooldown; pass send_now=true together with confirm_send_now=true",
                },
            }));
        }
        SendDisposition::Queue {
            cooldown_seconds: cd,
        } => {
            let db = Database::open_default().map_err(|e| e.to_string())?;
            let draft = db
                .get_draft(id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("draft not found: {id}"))?;
            let send_at = (chrono::Utc::now() + chrono::Duration::seconds(cd))
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            db.update_draft_send_after(&draft.id, &send_at)
                .map_err(|e| e.to_string())?;
            return Ok(json!({
                "sent": false,
                "status": "scheduled",
                "draft_id": draft.id,
                "send_after": send_at,
                "cooldown_seconds": cd,
                "ui": ui::draft_ui(&draft.account_id, &draft.id),
            }));
        }
        SendDisposition::Immediate => {}
    }

    // Explicit confirmed bypass: drive the silent shared send primitive. It runs
    // the Governor gate internally before any SMTP, returns structured JSON
    // (safe over the MCP stdio transport), and marks the local draft row sent so
    // a successful send can never leave the local DB at status=draft.
    let outcome = crate::commands::drafts::send_existing_draft(id, account_arg, backend)
        .await
        .map_err(|e| e.to_string())?;
    Ok(outcome.json)
}

async fn handle_move(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let uid = params
        .get("uid")
        .and_then(|v| v.as_u64())
        .ok_or("uid is required")? as u32;
    let to_folder = params
        .get("to_folder")
        .and_then(|v| v.as_str())
        .ok_or("to_folder is required")?;
    let from_folder = params
        .get("from_folder")
        .and_then(|v| v.as_str())
        .unwrap_or("INBOX");
    let account_arg = params.get("account").and_then(|v| v.as_str());

    let (_db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .map_err(|e| e.to_string())?;

    envelope_email_transport::imap::move_message(&mut client, uid, from_folder, to_folder)
        .await
        .map_err(|e| e.to_string())?;

    Ok(json!({
        "moved": true,
        "uid": uid,
        "from": from_folder,
        "to": to_folder,
        "ui": ui::message_ui(&creds.account.id, uid, to_folder),
    }))
}

async fn handle_flag(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let uid = params
        .get("uid")
        .and_then(|v| v.as_u64())
        .ok_or("uid is required")? as u32;
    let action = params
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or("action is required (add or remove)")?;
    let flag = params
        .get("flag")
        .and_then(|v| v.as_str())
        .ok_or("flag is required")?;
    let folder = params
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("INBOX");
    let account_arg = params.get("account").and_then(|v| v.as_str());

    let (_db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .map_err(|e| e.to_string())?;

    match action {
        "add" => {
            envelope_email_transport::imap::set_flag(&mut client, folder, uid, flag)
                .await
                .map_err(|e| e.to_string())?;
        }
        "remove" => {
            envelope_email_transport::imap::remove_flag(&mut client, folder, uid, flag)
                .await
                .map_err(|e| e.to_string())?;
        }
        _ => return Err("action must be 'add' or 'remove'".to_string()),
    }

    Ok(json!({
        "flagged": true,
        "uid": uid,
        "action": action,
        "flag": flag,
        "ui": ui::message_ui(&creds.account.id, uid, folder),
    }))
}

async fn handle_folders(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let account_arg = params.get("account").and_then(|v| v.as_str());

    let (_db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .map_err(|e| e.to_string())?;

    let stats = envelope_email_transport::imap::list_folder_stats(&mut client)
        .await
        .map_err(|e| e.to_string())?;

    Ok(json!({
        "folders": stats,
        "ui": ui::account_ui(&creds.account.id),
    }))
}

async fn handle_tag(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let uid = params
        .get("uid")
        .and_then(|v| v.as_u64())
        .ok_or("uid is required")? as u32;
    let folder = params
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("INBOX");
    let account_arg = params.get("account").and_then(|v| v.as_str());

    let (db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

    // Fetch message to get Message-ID
    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .map_err(|e| e.to_string())?;
    let message = envelope_email_transport::imap::fetch_message(&mut client, folder, uid)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("message {uid} not found in {folder}"))?;

    let message_id = message
        .message_id
        .as_deref()
        .ok_or("message has no Message-ID")?;

    // Set tags
    if let Some(tags) = params.get("tags").and_then(|v| v.as_array()) {
        for tag_val in tags {
            if let Some(tag) = tag_val.as_str() {
                db.add_tag(
                    &creds.account.id,
                    message_id,
                    tag,
                    Some(uid as i64),
                    Some(folder),
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }

    // Set scores
    if let Some(scores) = params.get("scores").and_then(|v| v.as_object()) {
        for (dimension, value) in scores {
            if let Some(val) = value.as_f64() {
                db.set_score(
                    &creds.account.id,
                    message_id,
                    dimension,
                    val,
                    Some(uid as i64),
                    Some(folder),
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }

    let current_tags = db
        .get_tags(&creds.account.id, message_id)
        .map_err(|e| e.to_string())?;
    let current_scores = db
        .get_scores(&creds.account.id, message_id)
        .map_err(|e| e.to_string())?;

    Ok(json!({
        "uid": uid,
        "message_id": message_id,
        "tags": current_tags,
        "scores": current_scores.iter().map(|s| json!({"dimension": s.dimension, "value": s.value})).collect::<Vec<_>>(),
        "ui": ui::message_ui(&creds.account.id, uid, folder),
    }))
}

async fn handle_contacts(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let action = params
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or("action is required")?;
    let account_arg = params.get("account").and_then(|v| v.as_str());

    let (db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

    match action {
        "list" => {
            let tag_filter = params.get("tag").and_then(|v| v.as_str());
            let contacts = db
                .list_contacts(&creds.account.id, tag_filter)
                .map_err(|e| e.to_string())?;
            Ok(Value::Array(
                contacts
                    .iter()
                    .map(|contact| ui::with_ui(contact, ui::account_ui(&creds.account.id)))
                    .collect(),
            ))
        }
        "show" => {
            let email = params
                .get("email")
                .and_then(|v| v.as_str())
                .ok_or("email is required for show")?;
            let contact = db
                .get_contact(&creds.account.id, email)
                .map_err(|e| e.to_string())?;
            Ok(ui::with_ui(&contact, ui::account_ui(&creds.account.id)))
        }
        "add" => {
            let email = params
                .get("email")
                .and_then(|v| v.as_str())
                .ok_or("email is required for add")?;
            let name = params.get("name").and_then(|v| v.as_str());
            let notes = params.get("notes").and_then(|v| v.as_str());
            let tag = params.get("tag").and_then(|v| v.as_str());

            let tags = match tag {
                Some(t) => serde_json::to_string(&vec![t]).unwrap_or_else(|_| "[]".to_string()),
                None => "[]".to_string(),
            };

            let now = chrono::Utc::now().to_rfc3339();
            let contact = envelope_email_store::Contact {
                id: uuid::Uuid::new_v4().to_string(),
                account_id: creds.account.id.clone(),
                email: email.to_string(),
                name: name.map(|s| s.to_string()),
                tags,
                notes: notes.map(|s| s.to_string()),
                message_count: 0,
                first_seen: Some(now.clone()),
                last_seen: Some(now.clone()),
                created_at: now.clone(),
                updated_at: now,
            };
            db.upsert_contact(&contact).map_err(|e| e.to_string())?;
            Ok(ui::with_ui(&contact, ui::account_ui(&creds.account.id)))
        }
        "tag" => {
            let email = params
                .get("email")
                .and_then(|v| v.as_str())
                .ok_or("email is required for tag")?;
            let tag = params
                .get("tag")
                .and_then(|v| v.as_str())
                .ok_or("tag is required")?;
            db.add_contact_tag(&creds.account.id, email, tag)
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "tagged": true,
                "email": email,
                "tag": tag,
                "ui": ui::account_ui(&creds.account.id),
            }))
        }
        "untag" => {
            let email = params
                .get("email")
                .and_then(|v| v.as_str())
                .ok_or("email is required for untag")?;
            let tag = params
                .get("tag")
                .and_then(|v| v.as_str())
                .ok_or("tag is required")?;
            db.remove_contact_tag(&creds.account.id, email, tag)
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "untagged": true,
                "email": email,
                "tag": tag,
                "ui": ui::account_ui(&creds.account.id),
            }))
        }
        _ => Err(format!("unknown contacts action: {action}")),
    }
}

// ── Config output ───────────────────────────────────────────────────

/// Print a ready-to-paste MCP config and runtime setup hints.
pub fn print_config() {
    println!("{}", serde_json::to_string_pretty(&config_json()).unwrap());
}

pub(crate) fn config_json() -> Value {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "envelope".to_string());
    let home = std::env::var("HOME").unwrap_or_default();
    let env = if home.is_empty() {
        json!({})
    } else {
        json!({ "HOME": home.clone() })
    };
    let server = json!({
        "command": exe.clone(),
        "args": ["mcp"],
        "env": env,
    });
    let draft_only_safety = "Send/reply tools default to draft-only for agent contexts; Envelope creates reviewable drafts and does not send live mail unless an operator explicitly opts into confirm-send, allowlisted-send, or autonomous-send.";
    let server_config = json!({ "mcpServers": { "envelope": server.clone() } });
    let server_compact = serde_json::to_string(&server).unwrap_or_default();
    let server_pretty = serde_json::to_string_pretty(&server_config).unwrap_or_default();
    let codex_snippet = codex_config_snippet(&exe, &home);

    json!({
        "mcpServers": {
            "envelope": server.clone()
        },
        "envelopeAgentSetup": {
            "sendSafety": draft_only_safety,
            "claudeCode": {
                "target": "Claude Code MCP server config",
                "commandPath": exe.clone(),
                "args": ["mcp"],
                "env": server["env"].clone(),
                "draftOnlySafety": draft_only_safety,
                "snippet": format!("claude mcp add-json envelope {}", shell_quote(&server_compact)),
                "command": "claude mcp add-json envelope '<paste the mcpServers.envelope object from this output>'",
                "config": { "mcpServers": { "envelope": server.clone() } }
            },
            "codex": {
                "target": "Codex MCP server config.toml",
                "commandPath": exe.clone(),
                "args": ["mcp"],
                "env": server["env"].clone(),
                "draftOnlySafety": draft_only_safety,
                "snippet": codex_snippet,
                "config": { "mcpServers": { "envelope": server.clone() } }
            },
            "hermes": {
                "target": "Hermes profile MCP/tool server config",
                "commandPath": exe,
                "args": ["mcp"],
                "env": server["env"].clone(),
                "draftOnlySafety": draft_only_safety,
                "snippet": server_pretty,
                "config": { "mcpServers": { "envelope": server } }
            }
        }
    })
}

fn codex_config_snippet(command_path: &str, home: &str) -> String {
    let mut snippet = format!(
        "[mcp_servers.envelope]\ncommand = {}\nargs = [\"mcp\"]",
        toml_string(command_path)
    );
    if !home.is_empty() {
        snippet.push_str(&format!(
            "\n\n[mcp_servers.envelope.env]\nHOME = {}",
            toml_string(home)
        ));
    }
    snippet
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

// ── Main loop ───────────────────────────────────────────────────────

fn read_mcp_message<R: BufRead>(reader: &mut R) -> io::Result<Option<String>> {
    loop {
        let mut first_line = String::new();
        let bytes = reader.read_line(&mut first_line)?;
        if bytes == 0 {
            return Ok(None);
        }

        let trimmed = first_line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }

        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                let content_length = value.trim().parse::<usize>().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid MCP Content-Length header",
                    )
                })?;

                loop {
                    let mut header_line = String::new();
                    let bytes = reader.read_line(&mut header_line)?;
                    if bytes == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "EOF while reading MCP headers",
                        ));
                    }
                    if header_line == "\r\n" || header_line == "\n" {
                        break;
                    }
                }

                let mut body = vec![0; content_length];
                reader.read_exact(&mut body)?;
                return String::from_utf8(body).map(Some).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "MCP body is not UTF-8")
                });
            }
        }

        // Compatibility for old Envelope toy clients that sent newline-delimited JSON-RPC.
        return Ok(Some(first_line));
    }
}

fn write_mcp_message<W: Write, T: Serialize>(writer: &mut W, value: &T) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_list_limit_accepts_default() {
        assert_eq!(
            validate_agent_list_limit(Some(&serde_json::json!(25))).unwrap(),
            25
        );
    }

    #[test]
    fn agent_list_limit_uses_default_when_absent() {
        assert_eq!(validate_agent_list_limit(None).unwrap(), 25);
    }

    #[test]
    fn agent_list_limit_accepts_max_cap() {
        assert_eq!(
            validate_agent_list_limit(Some(&serde_json::json!(1000))).unwrap(),
            1000
        );
    }

    #[test]
    fn agent_list_limit_rejects_above_cap() {
        let err = validate_agent_list_limit(Some(&serde_json::json!(1001)))
            .expect_err("limit above 1000 must be rejected");
        assert!(
            err.contains("limit") && err.contains("1000"),
            "expected limit/1000 error, got: {err}"
        );
    }

    #[test]
    fn agent_list_limit_rejects_zero() {
        let err = validate_agent_list_limit(Some(&serde_json::json!(0)))
            .expect_err("limit 0 must be rejected");
        assert!(
            err.contains("limit"),
            "expected limit error mentioning bound, got: {err}"
        );
    }

    #[test]
    fn agent_list_limit_rejects_present_wrong_json_types() {
        for raw in [
            serde_json::json!("100"),
            serde_json::json!(25.5),
            serde_json::json!(-1),
        ] {
            let err = validate_agent_list_limit(Some(&raw))
                .expect_err("present non-u64 limit must be rejected, not treated as absent");
            assert!(
                err.contains("limit") && err.contains("integer"),
                "expected limit/integer type error for {raw}, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn send_draft_denies_without_confirm_send() {
        // The MCP send_draft surface must default to draft-safe: without an
        // explicit confirm_send it returns a stable denial and never touches
        // SMTP/IMAP. This returns before any DB or network access.
        let params = serde_json::json!({ "draft_id": "abc-123" });
        let out = handle_send_draft(&params, CredentialBackend::File)
            .await
            .expect("denial path must not error");
        assert_eq!(out["status"], "denied");
        assert_eq!(out["error"]["code"], "confirm_send_required");
        // Regression: the old stub returned this code instead of sending. The
        // wired path must never advertise itself as unimplemented.
        assert_ne!(out["error"]["code"], "mcp_send_draft_not_wired");
    }

    #[tokio::test]
    async fn send_draft_requires_draft_id() {
        let params = serde_json::json!({ "confirm_send": true });
        let err = handle_send_draft(&params, CredentialBackend::File)
            .await
            .expect_err("missing draft_id must error");
        assert!(
            err.contains("draft_id"),
            "expected draft_id error, got: {err}"
        );
    }

    // Regression: MCP send and reply must NOT include a bare top-level
    // copy_source field. The canonical location is sent_mail.copy_source.
    // This test validates that the tool_list() contract schemas for reply and
    // send_draft advertise provider_sent_copy and client_appended_copy.
    #[test]
    fn mcp_reply_schema_advertises_sent_copy_source_fields() {
        let tools = tool_list();
        let entries = tools["tools"].as_array().expect("tools must be array");
        let reply = entries
            .iter()
            .find(|t| t["name"] == "reply")
            .expect("reply tool must exist in tool_list");

        // contractSchema must reference the surface which now has an explicit output_schema.
        assert!(
            reply.get("contractSchema").is_some(),
            "reply must have contractSchema"
        );

        // Verify the contract surface for reply includes the new output fields.
        let contract = crate::commands::contract::agent_contract();
        let surfaces = contract["surfaces"].as_array().expect("surfaces");
        let reply_surface = surfaces
            .iter()
            .find(|s| s["name"] == "reply")
            .expect("reply surface");
        let out_props = &reply_surface["output_schema"]["properties"];
        assert!(
            out_props.get("provider_sent_copy").is_some(),
            "reply output_schema must advertise provider_sent_copy"
        );
        assert!(
            out_props.get("client_appended_copy").is_some(),
            "reply output_schema must advertise client_appended_copy"
        );
        assert!(
            out_props.get("sent_mail").is_some(),
            "reply output_schema must advertise sent_mail (contains copy_source)"
        );
        assert!(
            out_props.get("parent_ui").is_some(),
            "reply output_schema must allow parent_ui emitted by handle_reply"
        );
    }

    #[test]
    fn mcp_send_draft_schema_advertises_sent_copy_source_fields() {
        let contract = crate::commands::contract::agent_contract();
        let surfaces = contract["surfaces"].as_array().expect("surfaces");
        let send_draft_surface = surfaces
            .iter()
            .find(|s| s["name"] == "send_draft")
            .expect("send_draft surface");
        let out_props = &send_draft_surface["output_schema"]["properties"];
        assert!(
            out_props.get("provider_sent_copy").is_some(),
            "send_draft output_schema must advertise provider_sent_copy"
        );
        assert!(
            out_props.get("client_appended_copy").is_some(),
            "send_draft output_schema must advertise client_appended_copy"
        );
        assert!(
            out_props.get("sent_mail").is_some(),
            "send_draft output_schema must advertise sent_mail (contains copy_source)"
        );
        for key in [
            "to",
            "subject",
            "imap_draft_deleted",
            "draft_ui",
            "error",
            "cooldown_seconds",
        ] {
            assert!(
                out_props.get(key).is_some(),
                "send_draft output_schema must allow actual output key {key}"
            );
        }
    }

    #[test]
    fn mcp_send_output_has_no_bare_top_level_copy_source() {
        // Validate that the MCP send surface output_schema does NOT advertise a
        // bare top-level copy_source field (it was removed as undocumented).
        let contract = crate::commands::contract::agent_contract();
        let surfaces = contract["surfaces"].as_array().expect("surfaces");
        let send_surface = surfaces
            .iter()
            .find(|s| s["name"] == "send")
            .expect("send surface");
        let out_props = &send_surface["output_schema"]["properties"];
        assert!(
            out_props.get("copy_source").is_none(),
            "send output_schema must not have bare top-level copy_source (use sent_mail.copy_source)"
        );
        // The canonical location must be present.
        assert!(
            out_props.get("sent_mail").is_some(),
            "send output_schema must advertise sent_mail (contains copy_source)"
        );
    }
}

pub async fn run(backend: CredentialBackend) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut out = stdout.lock();

    while let Some(message) = read_mcp_message(&mut input)? {
        let message = message.trim();
        if message.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(message) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse::error(None, -32700, format!("parse error: {e}"));
                write_mcp_message(&mut out, &resp)?;
                continue;
            }
        };

        let response = match request.method.as_str() {
            "initialize" => JsonRpcResponse::success(request.id, server_info()),

            "notifications/initialized" => continue,

            "tools/list" => JsonRpcResponse::success(request.id, tool_list()),

            "tools/call" => {
                let tool_name = request
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let arguments = request
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(json!({}));

                match handle_tool_call(tool_name, &arguments, backend.clone()).await {
                    Ok(result) => JsonRpcResponse::success(
                        request.id,
                        json!({
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string_pretty(&result).unwrap_or_default()
                            }]
                        }),
                    ),
                    Err(e) => JsonRpcResponse::success(
                        request.id,
                        json!({
                            "content": [{
                                "type": "text",
                                "text": format!("Error: {e}")
                            }],
                            "isError": true
                        }),
                    ),
                }
            }

            _ => JsonRpcResponse::error(
                request.id,
                -32601,
                format!("method not found: {}", request.method),
            ),
        };

        write_mcp_message(&mut out, &response)?;
    }

    Ok(())
}
