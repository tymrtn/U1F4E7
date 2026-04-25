// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Thread view: list threads, show thread by message-id.

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
    let db = state.db.lock().await;
    match db.list_threads(Some(&account_id), 100) {
        Ok(threads) => Json(json!({ "threads": threads })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

pub async fn show_by_message_id(
    State(state): State<AppState>,
    Path((account_id, message_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match db.find_thread_by_message_id(&message_id, &account_id) {
        Ok(Some(thread_id)) => match db.get_thread_messages(&thread_id) {
            Ok(messages) => Json(json!({
                "thread_id": thread_id,
                "messages": messages,
            }))
            .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
        },
        Ok(None) => (StatusCode::NOT_FOUND, "thread not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}
