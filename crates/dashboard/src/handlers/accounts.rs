// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Account management: list, add (with discovery), verify, delete.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::AppState;

pub async fn list(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.lock().await;
    match db.list_accounts() {
        Ok(accounts) => Json(json!({ "accounts": accounts })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {e}"),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct CreateAccountRequest {
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
}

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateAccountRequest>,
) -> impl IntoResponse {
    // Resolve SMTP/IMAP settings: explicit fields take precedence,
    // otherwise run auto-discovery on the domain.
    let domain = req
        .email
        .split('@')
        .nth(1)
        .unwrap_or("")
        .to_string();

    let (smtp_host, smtp_port, imap_host, imap_port) =
        if let (Some(sh), Some(sp), Some(ih), Some(ip)) =
            (&req.smtp_host, req.smtp_port, &req.imap_host, req.imap_port)
        {
            (sh.clone(), sp, ih.clone(), ip)
        } else {
            match envelope_email_transport::discover(&domain).await {
                Ok(result) => (
                    req.smtp_host.unwrap_or(result.smtp_host),
                    req.smtp_port.unwrap_or(result.smtp_port),
                    req.imap_host.unwrap_or(result.imap_host),
                    req.imap_port.unwrap_or(result.imap_port),
                ),
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        format!("discovery failed for {domain}: {e}"),
                    )
                        .into_response();
                }
            }
        };

    let passphrase = match envelope_email_store::credential_store::get_or_create_passphrase(
        state.backend,
    ) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("credential store error: {e}"),
            )
                .into_response();
        }
    };

    let _ = domain; // derived from username inside create_account
    let db = state.db.lock().await;
    let account = match db.create_account(
        req.display_name.as_deref().unwrap_or(&req.email),
        &req.email,
        &req.password,
        &smtp_host,
        smtp_port,
        &imap_host,
        imap_port,
        &passphrase,
    ) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("create account failed: {e}"),
            )
                .into_response();
        }
    };

    Json(json!({ "account": account })).into_response()
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().await;
    match db.delete_account(&id) {
        Ok(true) => Json(json!({ "deleted": id })).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "account not found").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {e}"),
        )
            .into_response(),
    }
}

#[derive(Serialize)]
pub struct VerifyResult {
    pub ok: bool,
    pub imap: bool,
    pub smtp: bool,
    pub error: Option<String>,
}

pub async fn verify(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Verify IMAP connectivity only (SMTP verify requires sending a
    // test email which is destructive; do that explicitly elsewhere).
    match state.get_or_create_imap(&id).await {
        Ok((client_arc, _creds)) => {
            // Touch the connection by locking briefly
            let _client = client_arc.lock().await;
            Json(VerifyResult {
                ok: true,
                imap: true,
                smtp: false, // not tested
                error: None,
            })
            .into_response()
        }
        Err(e) => Json(VerifyResult {
            ok: false,
            imap: false,
            smtp: false,
            error: Some(e.to_string()),
        })
        .into_response(),
    }
}

#[derive(Deserialize)]
pub struct DiscoverRequest {
    pub email: String,
}

pub async fn discover(Json(req): Json<DiscoverRequest>) -> impl IntoResponse {
    let domain = req.email.split('@').nth(1).unwrap_or("").to_string();
    match envelope_email_transport::discover(&domain).await {
        Ok(result) => Json(json!({
            "ok": true,
            "domain": result.domain,
            "smtp_host": result.smtp_host,
            "smtp_port": result.smtp_port,
            "smtp_source": result.smtp_source,
            "imap_host": result.imap_host,
            "imap_port": result.imap_port,
            "imap_source": result.imap_source,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}
