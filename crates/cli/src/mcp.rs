// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! MCP (Model Context Protocol) server for Envelope Email.
//!
//! Implements the MCP stdio transport: reads JSON-RPC requests from stdin,
//! dispatches to existing command functions, writes JSON-RPC responses to stdout.

use envelope_email_store::{CredentialBackend, Database};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

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

fn tool_list() -> Value {
    json!({
        "tools": [
            {
                "name": "inbox",
                "description": "List messages in a mailbox folder. Returns message summaries with UID, from, subject, date, and flags.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "folder": { "type": "string", "description": "IMAP folder name", "default": "INBOX" },
                        "limit": { "type": "integer", "description": "Maximum messages to return", "default": 25 },
                        "account": { "type": "string", "description": "Account email address (uses default if omitted)" }
                    }
                }
            },
            {
                "name": "read",
                "description": "Read a full email message by UID. Returns headers, text body, HTML body, and attachment metadata. Does not mark the message as read.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "uid": { "type": "integer", "description": "Message UID" },
                        "folder": { "type": "string", "description": "IMAP folder", "default": "INBOX" },
                        "account": { "type": "string", "description": "Account email address" }
                    },
                    "required": ["uid"]
                }
            },
            {
                "name": "search",
                "description": "Search messages using IMAP search syntax. Examples: 'FROM boss@company.com', 'SUBJECT invoice', 'UNSEEN'.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "IMAP search query" },
                        "folder": { "type": "string", "description": "IMAP folder", "default": "INBOX" },
                        "limit": { "type": "integer", "description": "Maximum results", "default": 25 },
                        "account": { "type": "string", "description": "Account email address" }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "send",
                "description": "Send an email. Supports text and HTML bodies, CC, BCC, reply-to, and file attachments.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "to": { "type": "string", "description": "Recipient email address" },
                        "subject": { "type": "string", "description": "Email subject" },
                        "body": { "type": "string", "description": "Plain text body" },
                        "html": { "type": "string", "description": "HTML body (optional, sent alongside text)" },
                        "cc": { "type": "string", "description": "CC recipient(s)" },
                        "bcc": { "type": "string", "description": "BCC recipient(s)" },
                        "reply_to": { "type": "string", "description": "Reply-To address" },
                        "from": { "type": "string", "description": "Override sender identity" },
                        "account": { "type": "string", "description": "Account email address" }
                    },
                    "required": ["to", "subject"]
                }
            },
            {
                "name": "reply",
                "description": "Reply to a message. Automatically sets In-Reply-To, References, and subject prefix.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "uid": { "type": "integer", "description": "UID of message to reply to" },
                        "body": { "type": "string", "description": "Reply text body" },
                        "html": { "type": "string", "description": "Reply HTML body" },
                        "reply_all": { "type": "boolean", "description": "Reply to all recipients", "default": false },
                        "folder": { "type": "string", "description": "IMAP folder of original message", "default": "INBOX" },
                        "account": { "type": "string", "description": "Account email address" }
                    },
                    "required": ["uid", "body"]
                }
            },
            {
                "name": "move_message",
                "description": "Move a message to another IMAP folder.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "uid": { "type": "integer", "description": "Message UID" },
                        "to_folder": { "type": "string", "description": "Destination folder" },
                        "from_folder": { "type": "string", "description": "Source folder", "default": "INBOX" },
                        "account": { "type": "string", "description": "Account email address" }
                    },
                    "required": ["uid", "to_folder"]
                }
            },
            {
                "name": "flag",
                "description": "Add or remove IMAP flags on a message. Common flags: \\Seen, \\Flagged, \\Answered, \\Draft, \\Deleted.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "uid": { "type": "integer", "description": "Message UID" },
                        "action": { "type": "string", "enum": ["add", "remove"], "description": "Add or remove the flag" },
                        "flag": { "type": "string", "description": "IMAP flag name (e.g. \\Seen, \\Flagged)" },
                        "folder": { "type": "string", "description": "IMAP folder", "default": "INBOX" },
                        "account": { "type": "string", "description": "Account email address" }
                    },
                    "required": ["uid", "action", "flag"]
                }
            },
            {
                "name": "folders",
                "description": "List IMAP folders with message counts (exists/unseen).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "account": { "type": "string", "description": "Account email address" }
                    }
                }
            },
            {
                "name": "tag",
                "description": "Set tags and scores on a message. Tags are freeform strings, scores are named dimensions with float values (0.0-1.0). Used by the rules engine.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "uid": { "type": "integer", "description": "Message UID" },
                        "tags": { "type": "array", "items": { "type": "string" }, "description": "Tags to add" },
                        "scores": { "type": "object", "additionalProperties": { "type": "number" }, "description": "Score dimensions (e.g. {\"urgent\": 0.9})" },
                        "folder": { "type": "string", "description": "IMAP folder", "default": "INBOX" },
                        "account": { "type": "string", "description": "Account email address" }
                    },
                    "required": ["uid"]
                }
            },
            {
                "name": "contacts",
                "description": "Manage contacts. Supports list, add, show, and tag operations.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["list", "add", "show", "tag", "untag"], "description": "Contact operation" },
                        "email": { "type": "string", "description": "Contact email address (required for add/show/tag/untag)" },
                        "name": { "type": "string", "description": "Contact name (for add)" },
                        "tag": { "type": "string", "description": "Tag to add/remove (for tag/untag), or filter (for list)" },
                        "notes": { "type": "string", "description": "Notes (for add)" },
                        "account": { "type": "string", "description": "Account email address" }
                    },
                    "required": ["action"]
                }
            },
            {
                "name": "accounts",
                "description": "List configured email accounts.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            }
        ]
    })
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
    serde_json::to_value(&accounts).map_err(|e| e.to_string())
}

async fn handle_inbox(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let account_arg = params.get("account").and_then(|v| v.as_str());
    let folder = params
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("INBOX");
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(25) as usize;

    let (_db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .map_err(|e| e.to_string())?;

    let messages = envelope_email_transport::imap::fetch_inbox(&mut client, folder, limit as u32)
        .await
        .map_err(|e| e.to_string())?;

    serde_json::to_value(&messages).map_err(|e| e.to_string())
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

    serde_json::to_value(&message).map_err(|e| e.to_string())
}

async fn handle_search(params: &Value, backend: CredentialBackend) -> Result<Value, String> {
    let query = params
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or("query is required")?;
    let folder = params
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("INBOX");
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(25) as usize;
    let account_arg = params.get("account").and_then(|v| v.as_str());

    let (_db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .map_err(|e| e.to_string())?;

    let messages = envelope_email_transport::imap::search(&mut client, folder, query, limit as u32)
        .await
        .map_err(|e| e.to_string())?;

    serde_json::to_value(&messages).map_err(|e| e.to_string())
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

    let (_db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
        .map_err(|e: anyhow::Error| e.to_string())?;

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

    Ok(json!({ "sent": true, "message_id": message_id }))
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

    let (_db, creds) = crate::commands::common::setup_credentials(account_arg, backend)
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

    Ok(json!({ "sent": true, "message_id": message_id, "in_reply_to": headers.in_reply_to }))
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

    Ok(json!({ "moved": true, "uid": uid, "from": from_folder, "to": to_folder }))
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

    Ok(json!({ "flagged": true, "uid": uid, "action": action, "flag": flag }))
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

    serde_json::to_value(&stats).map_err(|e| e.to_string())
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
            serde_json::to_value(&contacts).map_err(|e| e.to_string())
        }
        "show" => {
            let email = params
                .get("email")
                .and_then(|v| v.as_str())
                .ok_or("email is required for show")?;
            let contact = db
                .get_contact(&creds.account.id, email)
                .map_err(|e| e.to_string())?;
            serde_json::to_value(&contact).map_err(|e| e.to_string())
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
            serde_json::to_value(&contact).map_err(|e| e.to_string())
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
            Ok(json!({ "tagged": true, "email": email, "tag": tag }))
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
            Ok(json!({ "untagged": true, "email": email, "tag": tag }))
        }
        _ => Err(format!("unknown contacts action: {action}")),
    }
}

// ── Config output ───────────────────────────────────────────────────

/// Print a ready-to-paste MCP config snippet.
pub fn print_config() {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "envelope".to_string());

    let config = json!({
        "mcpServers": {
            "envelope": {
                "command": exe,
                "args": ["mcp"]
            }
        }
    });

    println!("{}", serde_json::to_string_pretty(&config).unwrap());
}

// ── Main loop ───────────────────────────────────────────────────────

pub async fn run(backend: CredentialBackend) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse::error(None, -32700, format!("parse error: {e}"));
                let mut out = stdout.lock();
                serde_json::to_writer(&mut out, &resp)?;
                out.write_all(b"\n")?;
                out.flush()?;
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

        let mut out = stdout.lock();
        serde_json::to_writer(&mut out, &response)?;
        out.write_all(b"\n")?;
        out.flush()?;
    }

    Ok(())
}
