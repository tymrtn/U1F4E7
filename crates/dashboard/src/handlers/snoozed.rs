// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Snoozed folder view + unsnooze action.

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
    match db.list_snoozed(Some(&account_id)) {
        Ok(items) => Json(json!({ "snoozed": items })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

pub async fn unsnooze(
    State(state): State<AppState>,
    Path((account_id, snoozed_id)): Path<(String, String)>,
) -> impl IntoResponse {
    // Look up the snooze record
    let snoozed = {
        let db = state.db.lock().await;
        match db.get_snoozed(&snoozed_id) {
            Ok(Some(s)) => s,
            Ok(None) => return (StatusCode::NOT_FOUND, "snooze record not found").into_response(),
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}"))
                    .into_response();
            }
        }
    };

    // Connect IMAP and move the message back
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    // Find the current UID in the snoozed folder (may have changed since move)
    let current_uid = if let Some(ref mid) = snoozed.message_id {
        let mid_clean = mid.trim_matches(|c| c == '<' || c == '>');
        match envelope_email_transport::imap::find_uid_by_message_id(
            &mut client,
            &snoozed.snoozed_folder,
            mid_clean,
        )
        .await
        {
            Ok(Some(uid)) => uid,
            _ => snoozed.uid,
        }
    } else {
        snoozed.uid
    };

    if let Err(e) = envelope_email_transport::imap::move_message(
        &mut client,
        current_uid,
        &snoozed.snoozed_folder,
        &snoozed.original_folder,
    )
    .await
    {
        state.evict_imap(&account_id).await;
        return (StatusCode::BAD_GATEWAY, format!("move back: {e}")).into_response();
    }

    drop(client);
    let db = state.db.lock().await;
    let _ = db.delete_snoozed(&snoozed.id);

    Json(json!({
        "ok": true,
        "id": snoozed_id,
        "moved_to": snoozed.original_folder,
    }))
    .into_response()
}
