// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Draft list (IMAP Drafts folder).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;

use crate::state::AppState;

pub async fn list(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
) -> impl IntoResponse {
    // Resolve email → account_id if needed
    let db = state.db.lock().await;
    let id = match db.find_account_by_email(&account_id) {
        Ok(Some(acct)) => acct.id,
        _ => account_id.clone(),
    };
    match db.list_drafts(&id, Some("draft"), 100, 0) {
        Ok(drafts) => Json(json!({ "drafts": drafts })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}
