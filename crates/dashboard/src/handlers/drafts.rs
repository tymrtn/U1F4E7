// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Draft list (IMAP Drafts folder).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use envelope_email_store::models::Account;
use envelope_email_store::{Database, StoreError};
use serde_json::json;

use crate::state::AppState;

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

    let dashboard_url = format!("/accounts/{}/drafts/{}", account.id, draft.id);

    Json(json!({
        "draft": draft,
        "account": account,
        "dashboard_url": dashboard_url,
    }))
    .into_response()
}

fn resolve_account(db: &Database, account_id: &str) -> Result<Option<Account>, StoreError> {
    if let Some(account) = db.get_account(account_id)? {
        return Ok(Some(account));
    }
    db.find_account_by_email(account_id)
}
