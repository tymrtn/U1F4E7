// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Folder listing with STATUS stats (exists/recent/unseen counts).
//! Retries once with a fresh IMAP connection on failure.

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
    // Try with pooled connection first
    let stats = match try_list_folder_stats(&state, &account_id).await {
        Ok(s) => s,
        Err(_first_err) => {
            // Evict stale connection + retry once with fresh
            state.evict_imap(&account_id).await;
            match try_list_folder_stats(&state, &account_id).await {
                Ok(s) => s,
                Err(e) => {
                    // Both attempts failed — return snoozed-only so the sidebar
                    // isn't completely empty. Include the error for the frontend.
                    let snoozed_count = {
                        let db = state.db.lock().await;
                        db.list_snoozed(Some(&account_id))
                            .map(|v| v.len() as u32)
                            .unwrap_or(0)
                    };
                    return Json(json!({
                        "folders": [],
                        "snoozed_virtual": {
                            "folder": "Snoozed",
                            "exists": snoozed_count,
                            "recent": 0,
                            "unseen": null,
                            "virtual": true,
                        },
                        "error": format!("IMAP connection failed: {e}"),
                    }))
                    .into_response();
                }
            }
        }
    };

    // Success — include snoozed count
    let snoozed_count = {
        let db = state.db.lock().await;
        db.list_snoozed(Some(&account_id))
            .map(|v| v.len() as u32)
            .unwrap_or(0)
    };

    Json(json!({
        "folders": stats,
        "snoozed_virtual": {
            "folder": "Snoozed",
            "exists": snoozed_count,
            "recent": 0,
            "unseen": null,
            "virtual": true,
        }
    }))
    .into_response()
}

async fn try_list_folder_stats(
    state: &AppState,
    account_id: &str,
) -> Result<Vec<envelope_email_store::models::FolderStats>, String> {
    let (client_arc, _creds) = state
        .get_or_create_imap(account_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut client = client_arc.lock().await;
    envelope_email_transport::imap::list_folder_stats(&mut client)
        .await
        .map_err(|e| e.to_string())
}
