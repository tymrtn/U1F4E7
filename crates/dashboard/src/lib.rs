// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Envelope Email dashboard — localhost web UI and REST API.
//!
//! Mounts under `http://localhost:<port>/` (default 3141). Provides:
//! - HTML + static assets bundled via `rust-embed` from `static/`
//! - REST API under `/api/*` for accounts, folders, messages, compose,
//!   drafts, snooze, threads
//!
//! Localhost-only by default — the CORS layer only trusts
//! `http://localhost:*` and `http://127.0.0.1:*` origins.

pub mod assets;
pub mod handlers;
pub mod state;

use std::net::SocketAddr;

use axum::Router;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use envelope_email_store::{CredentialBackend, Database};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::info;

use crate::assets::Assets;
use crate::state::AppState;

/// Start the dashboard server on the given port.
///
/// Opens the default database, builds an [`AppState`] with an IMAP connection
/// pool, mounts the router, and blocks serving until shutdown.
pub async fn serve(port: u16) -> anyhow::Result<()> {
    serve_with_backend(port, CredentialBackend::File).await
}

/// Start the dashboard server with a specific credential backend.
pub async fn serve_with_backend(
    port: u16,
    backend: CredentialBackend,
) -> anyhow::Result<()> {
    let db = Database::open_default().map_err(|e| anyhow::anyhow!("{e}"))?;
    let state = AppState::new(db, backend);

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| {
            let s = origin.to_str().unwrap_or("");
            s.starts_with("http://localhost:") || s.starts_with("http://127.0.0.1:")
        }))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION, header::ACCEPT]);

    let api = Router::new()
        // Accounts
        .route("/accounts", get(handlers::accounts::list).post(handlers::accounts::create))
        .route("/accounts/{id}", delete(handlers::accounts::delete))
        .route("/accounts/{id}/verify", post(handlers::accounts::verify))
        .route("/accounts/discover", post(handlers::accounts::discover))
        // Folders
        .route("/accounts/{id}/folders", get(handlers::folders::list))
        // Messages
        .route("/accounts/{id}/messages", get(handlers::messages::list))
        .route("/accounts/{id}/messages/{uid}", get(handlers::messages::read))
        .route("/accounts/{id}/messages/{uid}/flags", post(handlers::messages::flags))
        .route("/accounts/{id}/messages/{uid}/move", post(handlers::messages::mv))
        .route("/accounts/{id}/messages/{uid}", delete(handlers::messages::delete))
        .route("/accounts/{id}/search", get(handlers::messages::search))
        // Attachments
        .route(
            "/accounts/{id}/messages/{uid}/attachments/{filename}",
            get(handlers::attachments::download),
        )
        // Compose
        .route("/accounts/{id}/compose", post(handlers::compose::send))
        .route("/accounts/{id}/compose/reply", post(handlers::compose::reply))
        // Drafts
        .route("/accounts/{id}/drafts", get(handlers::drafts::list))
        // Snoozed
        .route("/accounts/{id}/snoozed", get(handlers::snoozed::list))
        .route(
            "/accounts/{id}/snoozed/{snoozed_id}/unsnooze",
            post(handlers::snoozed::unsnooze),
        )
        // Threads
        .route("/accounts/{id}/threads", get(handlers::threads::list))
        .route(
            "/accounts/{id}/threads/{message_id}",
            get(handlers::threads::show_by_message_id),
        )
        // Stats
        .route("/stats", get(handlers::stats::get));

    let app = Router::new()
        .route("/", get(index_page))
        .route("/static/{*path}", get(static_asset))
        .nest("/api", api)
        .layer(cors)
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;

    info!("dashboard listening on http://localhost:{port}");
    println!("Envelope dashboard running at http://localhost:{port}");

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {e}"))
}

// ── Static asset serving ─────────────────────────────────────────────

async fn index_page() -> Response {
    match Assets::get_file("index.html") {
        Some(bytes) => Html(String::from_utf8_lossy(&bytes).to_string()).into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "index.html missing from embedded assets",
        )
            .into_response(),
    }
}

async fn static_asset(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    match Assets::get_file(&path) {
        Some(bytes) => {
            let content_type = mime_guess::from_path(&path)
                .first_or_octet_stream()
                .to_string();
            Response::builder()
                .header(header::CONTENT_TYPE, content_type)
                .body(axum::body::Body::from(bytes))
                .unwrap()
        }
        None => (StatusCode::NOT_FOUND, format!("asset not found: {path}")).into_response(),
    }
}
