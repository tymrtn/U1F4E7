// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Message list + read + flag + move + delete + search.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_folder() -> String {
    "INBOX".to_string()
}

fn default_limit() -> u32 {
    50
}

pub async fn list(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    match envelope_email_transport::imap::fetch_inbox(&mut client, &q.folder, q.limit).await {
        Ok(msgs) => Json(json!({ "messages": msgs })).into_response(),
        Err(e) => {
            state.evict_imap(&account_id).await;
            (StatusCode::BAD_GATEWAY, format!("fetch: {e}")).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct ReadQuery {
    #[serde(default = "default_folder")]
    pub folder: String,
}

pub async fn read(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Query(q): Query<ReadQuery>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    match envelope_email_transport::imap::fetch_message(&mut client, &q.folder, uid).await {
        Ok(Some(msg)) => Json(json!({ "message": msg })).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "message not found").into_response(),
        Err(e) => {
            state.evict_imap(&account_id).await;
            (StatusCode::BAD_GATEWAY, format!("fetch: {e}")).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct FlagsRequest {
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

pub async fn flags(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Json(req): Json<FlagsRequest>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    for flag in &req.add {
        if let Err(e) =
            envelope_email_transport::imap::set_flag(&mut client, &req.folder, uid, flag).await
        {
            state.evict_imap(&account_id).await;
            return (StatusCode::BAD_GATEWAY, format!("set_flag {flag}: {e}")).into_response();
        }
    }
    for flag in &req.remove {
        if let Err(e) =
            envelope_email_transport::imap::remove_flag(&mut client, &req.folder, uid, flag).await
        {
            state.evict_imap(&account_id).await;
            return (StatusCode::BAD_GATEWAY, format!("remove_flag {flag}: {e}")).into_response();
        }
    }
    Json(json!({ "ok": true, "uid": uid, "added": req.add, "removed": req.remove })).into_response()
}

#[derive(Deserialize)]
pub struct MoveRequest {
    #[serde(default = "default_folder")]
    pub folder: String,
    pub to_folder: String,
}

pub async fn mv(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Json(req): Json<MoveRequest>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    match envelope_email_transport::imap::move_message(
        &mut client,
        uid,
        &req.folder,
        &req.to_folder,
    )
    .await
    {
        Ok(()) => {
            Json(json!({ "ok": true, "uid": uid, "moved_to": req.to_folder })).into_response()
        }
        Err(e) => {
            state.evict_imap(&account_id).await;
            (StatusCode::BAD_GATEWAY, format!("move: {e}")).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    #[serde(default = "default_folder")]
    pub folder: String,
}

pub async fn delete(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Query(q): Query<DeleteQuery>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    match envelope_email_transport::imap::delete_message(&mut client, &q.folder, uid).await {
        Ok(()) => Json(json!({ "ok": true, "uid": uid, "deleted_from": q.folder })).into_response(),
        Err(e) => {
            state.evict_imap(&account_id).await;
            (StatusCode::BAD_GATEWAY, format!("delete: {e}")).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default = "default_folder")]
    pub folder: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

pub async fn search(
    State(state): State<AppState>,
    Path(account_id): Path<String>,
    Query(sq): Query<SearchQuery>,
) -> impl IntoResponse {
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("IMAP: {e}")).into_response();
        }
    };
    let mut client = client_arc.lock().await;

    match envelope_email_transport::imap::search(&mut client, &sq.folder, &sq.q, sq.limit).await {
        Ok(results) => Json(json!({ "messages": results, "query": sq.q })).into_response(),
        Err(e) => {
            state.evict_imap(&account_id).await;
            (StatusCode::BAD_GATEWAY, format!("search: {e}")).into_response()
        }
    }
}
