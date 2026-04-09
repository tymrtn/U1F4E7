// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Folder listing with STATUS stats (exists/recent/unseen counts).

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
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    match envelope_email_transport::imap::list_folder_stats(&mut client).await {
        Ok(stats) => {
            // Also fetch snoozed count from DB (virtual "Snoozed" folder)
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
        Err(e) => {
            state.evict_imap(&account_id).await;
            (StatusCode::BAD_GATEWAY, format!("folder stats: {e}")).into_response()
        }
    }
}
