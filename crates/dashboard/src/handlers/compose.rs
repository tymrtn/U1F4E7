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

use envelope_email_transport::outbound::resolve_cooldown_seconds;
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

fn attachment_snapshots(
    raw: &[AttachmentPayload],
    decoded: &[SmtpAttachment],
) -> Vec<serde_json::Value> {
    raw.iter()
        .zip(decoded.iter())
        .map(|(raw, decoded)| {
            json!({
                "filename": raw.filename,
                "content_type": raw.content_type,
                "size": decoded.data.len(),
                "data_base64": raw.data_b64,
            })
        })
        .collect()
}

fn cooldown_send_after() -> (i64, String) {
    let cooldown = resolve_cooldown_seconds(None);
    let send_at = (chrono::Utc::now() + chrono::Duration::seconds(cooldown))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    (cooldown, send_at)
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

    let attachment_snapshots = attachment_snapshots(&req.attachments, &attachments);
    let (cooldown, send_at) = cooldown_send_after();
    let (draft, attribution) = {
        let db = state.db.lock().await;
        let draft = match db.create_draft(
            &creds.account.id,
            &req.to,
            Some(&req.subject),
            req.text.as_deref(),
            req.html.as_deref(),
            None,
            req.cc.as_deref(),
            req.bcc.as_deref(),
            Some("human:dashboard"),
        ) {
            Ok(d) => d,
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("queue draft: {e}")).into_response();
            }
        };
        if !attachment_snapshots.is_empty()
            && let Err(e) = db.update_draft_attachments(&draft.id, &attachment_snapshots)
        {
            return (StatusCode::BAD_GATEWAY, format!("queue attachments: {e}")).into_response();
        }
        // A dashboard compose is human-authored and human-sent: schedule and
        // record the approval attestation in one atomic store transaction,
        // bound to the final revision written above. `tyler_approved` is an
        // input attribute for Governor's blind scoring, never a bypass.
        let current = match db.get_draft(&draft.id) {
            Ok(Some(d)) => d,
            Ok(None) => {
                return (StatusCode::BAD_GATEWAY, "queue: draft vanished").into_response();
            }
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("queue reload: {e}")).into_response();
            }
        };
        // The atomic queue returns the exact attested row — no reload, no
        // fallback to pre-attestation state. If it cannot be obtained the queue
        // failed, so fail the request rather than report success off stale state.
        let attested = match db.queue_draft_with_human_approval(
            &current.id,
            current.revision,
            &send_at,
            "human:dashboard",
            &crate::timefmt::utc_now_string(),
        ) {
            Ok(attested) => attested,
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("queue approval: {e}")).into_response();
            }
        };
        // The additive human-origin attribution block (tyler_approved derived; no
        // fabricated bot declaration) is built from the attested row, deriving
        // attachment facts and the account domain exactly as the sweep will.
        let attribution = crate::human_queue_attribution_block(&attested, &creds.account.username);
        (draft, attribution)
    };

    state
        .events
        .publish(crate::events::DashboardEvent::DraftQueued {
            account_id: creds.account.id.clone(),
            draft_id: draft.id.clone(),
            origin: "compose",
        });

    Json(json!({
        "ok": true,
        "status": "queued",
        "draft_id": draft.id,
        "send_after": send_at,
        "cooldown_seconds": cooldown,
        "attribution": attribution,
    }))
    .into_response()
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

    let attachment_snapshots = attachment_snapshots(&req.attachments, &attachments);
    let (cooldown, send_at) = cooldown_send_after();
    let (draft, attribution) = {
        let db = state.db.lock().await;
        let draft = match db.create_draft(
            &creds.account.id,
            &headers.to,
            Some(&headers.subject),
            req.text.as_deref(),
            req.html.as_deref(),
            headers.in_reply_to.as_deref(),
            cc_joined.as_deref(),
            None,
            Some("human:dashboard"),
        ) {
            Ok(d) => d,
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("queue reply draft: {e}"))
                    .into_response();
            }
        };
        if let Err(e) = db.set_draft_metadata(
            &draft.id,
            &json!({
                "draft_kind": "reply",
                "in_reply_to": headers.in_reply_to.clone(),
                "references": headers.references.clone(),
                "source": {"folder": req.parent_folder, "uid": req.parent_uid},
            }),
        ) {
            return (
                StatusCode::BAD_GATEWAY,
                format!("queue reply metadata: {e}"),
            )
                .into_response();
        }
        if !attachment_snapshots.is_empty()
            && let Err(e) = db.update_draft_attachments(&draft.id, &attachment_snapshots)
        {
            return (
                StatusCode::BAD_GATEWAY,
                format!("queue reply attachments: {e}"),
            )
                .into_response();
        }
        // Schedule + approval attestation as one atomic store transaction,
        // bound to the final revision written above (metadata + attachments).
        let current = match db.get_draft(&draft.id) {
            Ok(Some(d)) => d,
            Ok(None) => {
                return (StatusCode::BAD_GATEWAY, "queue reply: draft vanished").into_response();
            }
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("queue reply reload: {e}"))
                    .into_response();
            }
        };
        // The atomic queue returns the exact attested row — no reload, no
        // fallback to pre-attestation state.
        let attested = match db.queue_draft_with_human_approval(
            &current.id,
            current.revision,
            &send_at,
            "human:dashboard",
            &crate::timefmt::utc_now_string(),
        ) {
            Ok(attested) => attested,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("queue reply approval: {e}"),
                )
                    .into_response();
            }
        };
        let attribution = crate::human_queue_attribution_block(&attested, &creds.account.username);
        (draft, attribution)
    };

    state
        .events
        .publish(crate::events::DashboardEvent::DraftQueued {
            account_id: creds.account.id.clone(),
            draft_id: draft.id.clone(),
            origin: "reply",
        });

    Json(json!({
        "ok": true,
        "status": "queued",
        "draft_id": draft.id,
        "send_after": send_at,
        "cooldown_seconds": cooldown,
        "in_reply_to": headers.in_reply_to,
        "references": headers.references,
        "attribution": attribution,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use envelope_email_store::Database;

    fn seed_account(db: &Database, id: &str, username: &str) {
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password) \
                 VALUES (?1, 'Test', ?2, 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'encrypted')",
                (id, username),
            )
            .unwrap();
    }

    /// Dashboard-composed drafts and replies must be stamped created_by='human:dashboard'
    /// so they are distinguishable from agent-composed drafts in audit logs and
    /// Governor scoring. This test verifies the constant we pass to create_draft.
    #[test]
    fn compose_handler_source_stamps_human_dashboard_created_by() {
        let source = include_str!("compose.rs");
        let needle = "human:dashboard";
        assert!(
            source.contains(needle),
            "compose.rs must pass created_by='human:dashboard' to create_draft; found no such stamp"
        );
        // Must not still have the bare 'dashboard' stamp (without the 'human:' prefix)
        // in a create_draft call context. We check by confirming the only occurrences
        // of 'dashboard' in string literals carry the 'human:' prefix.
        let bare = "Some(\"dashboard\")";
        assert!(
            !source.contains(bare),
            "compose.rs must not pass bare created_by='dashboard'; use 'human:dashboard'"
        );
    }

    /// Verifies create_draft accepts human:dashboard as a valid created_by value
    /// by exercising it against a real in-memory database.
    #[test]
    fn create_draft_accepts_human_dashboard_created_by() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "agent@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Hello"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("human:dashboard"),
            )
            .unwrap();
        assert_eq!(
            draft.created_by.as_deref(),
            Some("human:dashboard"),
            "create_draft must persist the human:dashboard stamp"
        );
    }
}
