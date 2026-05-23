// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! MCP (Model Context Protocol) server for Envelope Email.
//!
//! Implements the MCP stdio transport: reads JSON-RPC requests from stdin,
//! dispatches to existing command functions, writes JSON-RPC responses to stdout.

use crate::commands::contract::{DEFAULT_AGENT_LIST_LIMIT, MAX_AGENT_LIST_LIMIT};
use crate::commands::ui;
use envelope_email_store::{CredentialBackend, Database, Event};
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
fn validate_agent_list_limit(raw: Option<u64>) -> Result<u32, String> {
    let value = raw.unwrap_or(DEFAULT_AGENT_LIST_LIMIT as u64);
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
    let limit = validate_agent_list_limit(params.get("limit").and_then(|v| v.as_u64()))?;
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
    let limit = validate_agent_list_limit(params.get("limit").and_then(|v| v.as_u64()))?;
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
            return Ok(json!({
                "sent": false,
                "status": "drafted",
                "send_mode": send_mode,
                "draft_id": draft.id,
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
        &[],
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(json!({
        "sent": true,
        "message_id": message_id,
        "ui": ui::account_ui(&creds.account.id),
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
            return Ok(json!({
                "sent": false,
                "status": "drafted",
                "send_mode": send_mode,
                "draft_id": draft.id,
                "in_reply_to": headers.in_reply_to,
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
        &[],
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(json!({
        "sent": true,
        "message_id": message_id,
        "in_reply_to": headers.in_reply_to,
        "ui": ui::message_ui(&creds.account.id, uid, folder),
    }))
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
        assert_eq!(validate_agent_list_limit(Some(25)).unwrap(), 25);
    }

    #[test]
    fn agent_list_limit_uses_default_when_absent() {
        assert_eq!(validate_agent_list_limit(None).unwrap(), 25);
    }

    #[test]
    fn agent_list_limit_accepts_max_cap() {
        assert_eq!(validate_agent_list_limit(Some(1000)).unwrap(), 1000);
    }

    #[test]
    fn agent_list_limit_rejects_above_cap() {
        let err =
            validate_agent_list_limit(Some(1001)).expect_err("limit above 1000 must be rejected");
        assert!(
            err.contains("limit") && err.contains("1000"),
            "expected limit/1000 error, got: {err}"
        );
    }

    #[test]
    fn agent_list_limit_rejects_zero() {
        let err = validate_agent_list_limit(Some(0)).expect_err("limit 0 must be rejected");
        assert!(
            err.contains("limit"),
            "expected limit error mentioning bound, got: {err}"
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
