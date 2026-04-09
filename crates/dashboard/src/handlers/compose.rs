// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Compose new messages, replies, and reply-all (with attachments).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use serde_json::json;

use envelope_email_transport::SmtpSender;
use envelope_email_transport::reply::build_reply_all_headers;
use envelope_email_transport::reply::build_reply_headers;
use envelope_email_transport::smtp::Attachment as SmtpAttachment;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct AttachmentPayload {
    pub filename: String,
    pub content_type: String,
    pub data_b64: String,
}

fn decode_attachments(raw: &[AttachmentPayload]) -> Result<Vec<SmtpAttachment>, String> {
    raw.iter()
        .map(|a| {
            let data = B64
                .decode(&a.data_b64)
                .map_err(|e| format!("base64 decode {}: {e}", a.filename))?;
            Ok(SmtpAttachment {
                filename: a.filename.clone(),
                content_type: a.content_type.clone(),
                data,
            })
        })
        .collect()
}

#[derive(Deserialize)]
pub struct ComposeRequest {
    pub to: String,
    pub subject: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub cc: Option<String>,
    #[serde(default)]
    pub bcc: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentPayload>,
}

pub async fn send(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Json(req): Json<ComposeRequest>,
) -> impl IntoResponse {
    let (_client, creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("resolve account: {e}")).into_response();
        }
    };

    let attachments = match decode_attachments(&req.attachments) {
        Ok(a) => a,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    match SmtpSender::send(
        &creds,
        &req.to,
        &req.subject,
        req.text.as_deref(),
        req.html.as_deref(),
        req.cc.as_deref(),
        req.bcc.as_deref(),
        req.reply_to.as_deref(),
        None,
        None,
        &attachments,
    )
    .await
    {
        Ok(message_id) => Json(json!({ "ok": true, "message_id": message_id })).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("send: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
pub struct ReplyRequest {
    pub parent_uid: u32,
    #[serde(default = "default_folder")]
    pub parent_folder: String,
    #[serde(default)]
    pub reply_all: bool,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentPayload>,
}

fn default_folder() -> String {
    "INBOX".to_string()
}

pub async fn reply(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Json(req): Json<ReplyRequest>,
) -> impl IntoResponse {
    let (client_arc, creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };

    // Fetch parent
    let parent = {
        let mut client = client_arc.lock().await;
        match envelope_email_transport::imap::fetch_message(
            &mut client,
            &req.parent_folder,
            req.parent_uid,
        )
        .await
        {
            Ok(Some(m)) => m,
            Ok(None) => return (StatusCode::NOT_FOUND, "parent not found").into_response(),
            Err(e) => {
                state.evict_imap(&account_id).await;
                return (StatusCode::BAD_GATEWAY, format!("fetch parent: {e}")).into_response();
            }
        }
    };

    let headers = if req.reply_all {
        build_reply_all_headers(&parent, &creds.account.username)
    } else {
        build_reply_headers(&parent)
    };

    let attachments = match decode_attachments(&req.attachments) {
        Ok(a) => a,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    let cc_joined = if headers.cc.is_empty() {
        None
    } else {
        Some(headers.cc.join(", "))
    };

    match SmtpSender::send(
        &creds,
        &headers.to,
        &headers.subject,
        req.text.as_deref(),
        req.html.as_deref(),
        cc_joined.as_deref(),
        None,
        None,
        headers.in_reply_to.as_deref(),
        Some(&headers.references),
        &attachments,
    )
    .await
    {
        Ok(message_id) => Json(json!({
            "ok": true,
            "message_id": message_id,
            "in_reply_to": headers.in_reply_to,
            "references": headers.references,
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("send reply: {e}")).into_response(),
    }
}
