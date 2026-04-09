// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Aggregate dashboard stats (account count, draft count, etc.).

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde_json::json;

use crate::state::AppState;

pub async fn get(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.lock().await;
    let account_count = db.list_accounts().map(|v| v.len()).unwrap_or(0);
    let snoozed_count = db.list_snoozed(None).map(|v| v.len()).unwrap_or(0);

    Json(json!({
        "accounts": account_count,
        "snoozed": snoozed_count,
    }))
}
