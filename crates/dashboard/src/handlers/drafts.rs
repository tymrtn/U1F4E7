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

    let draft = {
        let db = state.db.lock().await;
        match ensure_draft_account(&db, &account_id, &draft_id) {
            Ok(draft) => draft,
            Err(e) => return draft_error(e),
        }
    };

    let (_client_arc, creds) = match state.get_or_create_imap(&draft.account_id).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("credentials: {e}")).into_response(),
    };

    let subject = draft.subject.as_deref().unwrap_or("");
    match envelope_email_transport::SmtpSender::send_simple(
        &creds,
        &draft.to_addr,
        subject,
        draft.text_content.as_deref(),
        draft.html_content.as_deref(),
        draft.cc_addr.as_deref(),
        draft.bcc_addr.as_deref(),
        draft.reply_to.as_deref(),
    )
    .await
    {
        Ok(message_id) => {
            let db = state.db.lock().await;
            match db.mark_draft_sent(&draft.id, Some(&message_id)) {
                Ok(()) => Json(json!({
                    "draft_id": draft.id,
                    "status": "sent",
                    "message_id": message_id
                }))
                .into_response(),
                Err(e) => draft_error(e),
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("SMTP: {e}")).into_response(),
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
}
