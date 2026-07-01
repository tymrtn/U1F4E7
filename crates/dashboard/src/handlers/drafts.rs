// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Draft list and dashboard operator actions.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use envelope_email_store::DraftStatus;
use envelope_email_store::models::Account;
use envelope_email_store::{Database, StoreError};
use envelope_email_transport::outbound::{
    OUTBOX_COOLDOWN_REASON, OUTBOX_COOLDOWN_REASON_CODE, resolve_cooldown_seconds,
};
use serde::Deserialize;
use serde_json::json;

use crate::state::AppState;
use crate::ui_paths::draft_dashboard_path;

fn dashboard_base_url(headers: &HeaderMap) -> Option<String> {
    let host = headers.get("host")?.to_str().ok()?.trim();
    if host.is_empty() {
        return None;
    }
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http");
    Some(format!("{proto}://{host}"))
}

fn draft_dashboard_url(headers: &HeaderMap, account_id: &str, draft_id: &str) -> Option<String> {
    Some(format!(
        "{}{}",
        dashboard_base_url(headers)?,
        draft_dashboard_path(account_id, draft_id)
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftEditRequest {
    pub to_addr: Option<String>,
    pub cc_addr: Option<String>,
    pub bcc_addr: Option<String>,
    pub subject: Option<String>,
    pub text_content: Option<String>,
    pub html_content: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftBlockRequest {
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftSendRequest {
    #[serde(default)]
    pub confirm: bool,
    /// Optional override for the outbox cooldown (seconds). Omitted → the shared
    /// default cooldown. Negative values clamp to zero. There is intentionally no
    /// immediate-SMTP dashboard bypass: the queued draft is transmitted later by
    /// the shared scheduled-send sweep, after the Governor gate.
    #[serde(default)]
    pub cooldown_seconds: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    let account = match resolve_account(&db, &account_id) {
        Ok(Some(account)) => account,
        Ok(None) => return (StatusCode::NOT_FOUND, "account not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
    };

    match db.list_drafts(&account.id, Some("draft"), 100, 0) {
        Ok(drafts) => Json(json!({ "drafts": drafts })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

pub async fn show(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((account_id, draft_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    let account = match resolve_account(&db, &account_id) {
        Ok(Some(account)) => account,
        Ok(None) => return (StatusCode::NOT_FOUND, "account not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
    };

    let draft = match db.get_draft(&draft_id) {
        Ok(Some(draft)) if draft.account_id == account.id => draft,
        Ok(Some(_)) | Ok(None) => {
            return (StatusCode::NOT_FOUND, "draft not found").into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
    };

    let dashboard_path = draft_dashboard_path(&account.id, &draft.id);
    let dashboard_url = draft_dashboard_url(&headers, &account.id, &draft.id);

    Json(json!({
        "draft": draft,
        "account": account,
        "dashboard_path": dashboard_path,
        "dashboard_url": dashboard_url,
        "review_url": dashboard_url,
        "metadata": {
            "dashboard_path": dashboard_path,
            "dashboard_url": dashboard_url,
            "review_url": dashboard_url,
        },
    }))
    .into_response()
}

fn resolve_account(db: &Database, account_id: &str) -> Result<Option<Account>, StoreError> {
    if let Some(account) = db.get_account(account_id)? {
        return Ok(Some(account));
    }
    db.find_account_by_email(account_id)
}

pub async fn approve(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match ensure_draft_account(&db, &account_id, &draft_id)
        .and_then(|_| db.update_draft_status(&draft_id, DraftStatus::Draft))
        .and_then(|_| db.get_draft(&draft_id))
    {
        Ok(Some(draft)) => Json(json!({ "draft": draft, "status": "approved" })).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "draft not found").into_response(),
        Err(e) => draft_error(e),
    }
}

pub async fn edit(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
    Json(req): Json<DraftEditRequest>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match ensure_draft_account(&db, &account_id, &draft_id).and_then(|draft| {
        db.update_draft_content(
            &draft.id,
            req.to_addr.as_deref(),
            req.cc_addr.as_deref().or(draft.cc_addr.as_deref()),
            req.bcc_addr.as_deref().or(draft.bcc_addr.as_deref()),
            req.subject.as_deref(),
            req.text_content.as_deref(),
            req.html_content.as_deref(),
        )
    }) {
        Ok(draft) => Json(json!({ "draft": draft, "status": "edited" })).into_response(),
        Err(e) => draft_error(e),
    }
}

pub async fn discard(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match ensure_draft_account(&db, &account_id, &draft_id)
        .and_then(|_| db.discard_draft(&draft_id))
    {
        Ok(true) => Json(json!({ "draft_id": draft_id, "status": "discarded" })).into_response(),
        Ok(false) => (StatusCode::CONFLICT, "draft is not discardable").into_response(),
        Err(e) => draft_error(e),
    }
}

pub async fn block(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
    Json(req): Json<DraftBlockRequest>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match ensure_draft_account(&db, &account_id, &draft_id)
        .and_then(|_| db.update_draft_status(&draft_id, DraftStatus::Blocked))
        .and_then(|_| db.get_draft(&draft_id))
    {
        Ok(Some(draft)) => Json(json!({
            "draft": draft,
            "status": "blocked",
            "reason": req.reason.unwrap_or_else(|| "changes requested".to_string())
        }))
        .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "draft not found").into_response(),
        Err(e) => draft_error(e),
    }
}

/// Queue an approved draft into the Envelope outbox cooldown instead of
/// transmitting it inline.
///
/// Setting `send_after` (and ensuring the draft stays in sweep-eligible `draft`
/// status) hands the real send to the shared scheduled-send sweep
/// (`run_scheduled_send_sweep`), which applies persisted attachment
/// snapshots, reply threading headers, and the fail-closed Governor gate before
/// any SMTP — exactly like CLI `draft send` and MCP `send_draft`. The draft is
/// never marked sent here (that would drop it from the sweep and could strand it
/// unsent) and never discarded.
///
/// Returns the resolved `send_after` timestamp.
fn queue_draft_for_outbox(
    db: &Database,
    draft: &envelope_email_store::Draft,
    cooldown_seconds: i64,
) -> envelope_email_store::errors::Result<String> {
    // `list_drafts_due_for_send` only selects `status='draft'`; a
    // pending-review draft approved by a human must be promoted so the sweep can
    // pick it up. Do not silently override blocked/discarded/sent states: those
    // represent explicit operator decisions or terminal states.
    match draft.status {
        DraftStatus::Draft => {}
        DraftStatus::PendingReview => {
            db.update_draft_status(&draft.id, DraftStatus::Draft)?;
        }
        DraftStatus::Blocked | DraftStatus::Discarded | DraftStatus::Sent => {
            return Err(envelope_email_store::StoreError::DraftNotEditable(
                draft.status.as_str().to_string(),
            ));
        }
    }
    let send_at = (chrono::Utc::now() + chrono::Duration::seconds(cooldown_seconds))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    db.update_draft_send_after(&draft.id, &send_at)?;
    Ok(send_at)
}

pub async fn send(
    State(state): State<AppState>,
    Path((account_id, draft_id)): Path<(String, String)>,
    Json(req): Json<DraftSendRequest>,
) -> impl IntoResponse {
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            "draft send mutates the outside world; send confirm=true",
        )
            .into_response();
    }

    let db = state.db.lock().await;
    let draft = match ensure_draft_account(&db, &account_id, &draft_id) {
        Ok(draft) => draft,
        Err(e) => return draft_error(e),
    };

    if draft.status == DraftStatus::Sent {
        return (StatusCode::CONFLICT, "draft already sent").into_response();
    }

    // Do NOT transmit immediately. Queue into the outbox cooldown so the shared
    // scheduled-send sweep performs the real SMTP send (with attachments,
    // threading, and the Governor gate). This keeps the dashboard send path
    // aligned with CLI/MCP/scheduled outbound semantics; there is no immediate
    // dashboard SMTP bypass.
    let cooldown = resolve_cooldown_seconds(req.cooldown_seconds);
    match queue_draft_for_outbox(&db, &draft, cooldown) {
        Ok(send_after) => Json(json!({
            "draft_id": draft.id,
            "sent": false,
            "status": "queued",
            "send_after": send_after,
            "cooldown_seconds": cooldown,
            "queued_reason_code": OUTBOX_COOLDOWN_REASON_CODE,
            "queued_reason": OUTBOX_COOLDOWN_REASON,
        }))
        .into_response(),
        Err(e) => draft_error(e),
    }
}

fn ensure_draft_account(
    db: &envelope_email_store::Database,
    account_id: &str,
    draft_id: &str,
) -> envelope_email_store::errors::Result<envelope_email_store::Draft> {
    let resolved = match db.find_account_by_email(account_id)? {
        Some(acct) => acct.id,
        None => account_id.to_string(),
    };
    let draft = db
        .get_draft(draft_id)?
        .ok_or_else(|| envelope_email_store::StoreError::DraftNotFound(draft_id.to_string()))?;
    if draft.account_id != resolved {
        return Err(envelope_email_store::StoreError::DraftNotFound(
            draft_id.to_string(),
        ));
    }
    Ok(draft)
}

fn draft_error(e: envelope_email_store::StoreError) -> axum::response::Response {
    match e {
        envelope_email_store::StoreError::DraftNotFound(_) => {
            (StatusCode::NOT_FOUND, format!("{e}")).into_response()
        }
        envelope_email_store::StoreError::DraftNotEditable(_) => {
            (StatusCode::CONFLICT, format!("{e}")).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use envelope_email_store::{Database, DraftStatus};

    #[test]
    fn draft_operator_primitives_are_account_scoped() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "a@example.com");
        seed_account(&db, "acc2", "b@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Old"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();

        assert!(super::ensure_draft_account(&db, "acc1", &draft.id).is_ok());
        assert!(super::ensure_draft_account(&db, "a@example.com", &draft.id).is_ok());
        assert!(super::ensure_draft_account(&db, "acc2", &draft.id).is_err());
        db.update_draft_status(&draft.id, DraftStatus::Draft)
            .unwrap();
        let edited = db
            .update_draft_content(
                &draft.id,
                Some("new@example.com"),
                None,
                None,
                Some("New"),
                Some("New body"),
                None,
            )
            .unwrap();
        assert_eq!(edited.to_addr, "new@example.com");
        assert_eq!(edited.subject.as_deref(), Some("New"));
        assert!(db.discard_draft(&draft.id).unwrap());
    }

    fn seed_account(db: &Database, id: &str, username: &str) {
        db.conn().execute("INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password) VALUES (?1, 'Test', ?2, 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'encrypted')", (id, username)).unwrap();
    }

    /// Regression for issue #68: dashboard draft send must queue into the outbox
    /// cooldown (handing the real send to the shared scheduled-send sweep) rather
    /// than transmitting inline. It must never mark the draft sent itself, never
    /// discard it, and must preserve persisted attachments and reply threading.
    #[test]
    fn dashboard_send_queues_into_outbox_without_inline_transmit() {
        let db = Database::open_memory().unwrap();
        seed_account(&db, "acc1", "agent@example.com");
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Re: order"),
                Some("Body"),
                None,
                Some("parent-msg-id"),
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        // Persisted attachment snapshot + reply-threading metadata, as a
        // contextual reply draft would carry them.
        db.update_draft_attachments(
            &draft.id,
            &[serde_json::json!({
                "filename": "packet.pdf",
                "content_type": "application/pdf",
                "size": 5,
                "data_base64": "aGVsbG8=",
            })],
        )
        .unwrap();
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({
                "in_reply_to": "<parent-msg-id>",
                "references": ["<root-id>", "<parent-msg-id>"],
            }),
        )
        .unwrap();
        let draft = db.get_draft(&draft.id).unwrap().unwrap();

        let send_at = super::queue_draft_for_outbox(&db, &draft, 120).unwrap();

        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        // Queued, not sent: sweep-eligible status and a future send_after, with no
        // sent_at stamped by the dashboard path.
        assert_eq!(reloaded.status, DraftStatus::Draft);
        assert_eq!(reloaded.send_after.as_deref(), Some(send_at.as_str()));
        assert!(reloaded.sent_at.is_none());

        // A 120s cooldown means the sweep does not pick it up immediately, proving
        // there is no inline transmission.
        let due_now: Vec<_> = db
            .list_drafts_due_for_send()
            .unwrap()
            .into_iter()
            .filter(|d| d.id == reloaded.id)
            .collect();
        assert!(
            due_now.is_empty(),
            "queued draft must not be due before cooldown"
        );

        // Attachments and threading survive for the shared sweep send path.
        assert_eq!(reloaded.attachments.len(), 1);
        assert_eq!(reloaded.attachments[0]["filename"], "packet.pdf");
        let (irt, refs) = (
            reloaded
                .metadata
                .as_ref()
                .and_then(|m| m.get("in_reply_to"))
                .and_then(|v| v.as_str()),
            reloaded
                .metadata
                .as_ref()
                .and_then(|m| m.get("references"))
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
        );
        assert_eq!(irt, Some("<parent-msg-id>"));
        assert_eq!(refs, 2);

        // Once the cooldown elapses the shared sweep query selects it.
        let past = (chrono::Utc::now() - chrono::Duration::seconds(5))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        db.update_draft_send_after(&reloaded.id, &past).unwrap();
        let due_later: Vec<_> = db
            .list_drafts_due_for_send()
            .unwrap()
            .into_iter()
            .filter(|d| d.id == reloaded.id)
            .collect();
        assert_eq!(due_later.len(), 1, "due draft must reach the sweep");
    }

    /// Queueing must promote a pending-review draft to sweep-eligible `draft`
    /// status (so the sweep can find it) without ever marking it sent.
    #[test]
    fn dashboard_send_promotes_pending_review_draft_to_sweep_eligible() {
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
                Some("agent"),
            )
            .unwrap();
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();
        let draft = db.get_draft(&draft.id).unwrap().unwrap();

        super::queue_draft_for_outbox(&db, &draft, 0).unwrap();
        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(reloaded.status, DraftStatus::Draft);
        assert!(reloaded.sent_at.is_none());
    }

    /// Queueing must not override explicit blocked/discarded terminal operator
    /// states. A blocked draft means changes were requested; it should not be
    /// silently promoted into the outbox by an API call.
    #[test]
    fn dashboard_send_does_not_queue_blocked_or_discarded_drafts() {
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
                Some("agent"),
            )
            .unwrap();

        db.update_draft_status(&draft.id, DraftStatus::Blocked)
            .unwrap();
        let blocked = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(super::queue_draft_for_outbox(&db, &blocked, 0).is_err());
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::Blocked
        );

        db.update_draft_status(&draft.id, DraftStatus::Discarded)
            .unwrap();
        let discarded = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(super::queue_draft_for_outbox(&db, &discarded, 0).is_err());
        assert_eq!(
            db.get_draft(&draft.id).unwrap().unwrap().status,
            DraftStatus::Discarded
        );
    }

    /// The dashboard send handler source must not call any direct simple-send
    /// path — that bypasses the cooldown, Governor gate, attachments, and
    /// threading. This guards against a regression reintroducing it.
    #[test]
    fn dashboard_send_does_not_call_direct_simple_send() {
        let source = include_str!("drafts.rs");
        // Build the call-form needle by concatenation so this assertion's own
        // source does not match it.
        let needle = format!("send_simple{}", "(");
        assert!(
            !source.contains(&needle),
            "dashboard draft send must not call the direct simple-send path"
        );
    }
}
