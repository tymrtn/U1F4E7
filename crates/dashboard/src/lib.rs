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
pub async fn serve_with_backend(port: u16, backend: CredentialBackend) -> anyhow::Result<()> {
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

    let app = dashboard_router(state.clone()).layer(cors);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;

    info!("dashboard listening on http://localhost:{port}");
    println!("Envelope dashboard running at http://localhost:{port}");
    println!("Background unsnooze + scheduled-send sweep running every 60s");

    // Spawn background ticker (checks every 60s for due snoozes and scheduled sends)
    tokio::spawn(async move {
        let ticker_state = state;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            if let Err(e) = run_unsnooze_sweep(&ticker_state).await {
                tracing::warn!("unsnooze sweep error: {e}");
            }
            if let Err(e) = run_scheduled_send_sweep(&ticker_state).await {
                tracing::warn!("scheduled send sweep error: {e}");
            }
        }
    });

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {e}"))
}

fn dashboard_router(state: AppState) -> Router {
    let api = Router::new()
        // Accounts
        .route(
            "/accounts",
            get(handlers::accounts::list).post(handlers::accounts::create),
        )
        .route("/accounts/{id}", delete(handlers::accounts::delete))
        .route("/accounts/{id}/verify", post(handlers::accounts::verify))
        .route("/accounts/discover", post(handlers::accounts::discover))
        // Folders
        .route("/accounts/{id}/folders", get(handlers::folders::list))
        // Messages
        .route("/accounts/{id}/messages", get(handlers::messages::list))
        .route(
            "/accounts/{id}/messages/{uid}",
            get(handlers::messages::read),
        )
        .route(
            "/accounts/{id}/messages/{uid}/flags",
            post(handlers::messages::flags),
        )
        .route(
            "/accounts/{id}/messages/{uid}/move",
            post(handlers::messages::mv),
        )
        .route(
            "/accounts/{id}/messages/{uid}",
            delete(handlers::messages::delete),
        )
        .route("/accounts/{id}/search", get(handlers::messages::search))
        // Rules
        .route("/accounts/{id}/rules", get(handlers::rules::list))
        .route(
            "/accounts/{id}/rules/run",
            post(handlers::rules::run_enabled),
        )
        .route(
            "/accounts/{id}/rules/test/{uid}",
            get(handlers::rules::test_message),
        )
        // Attachments
        .route(
            "/accounts/{id}/messages/{uid}/attachments/{filename}",
            get(handlers::attachments::download),
        )
        // Compose
        .route("/accounts/{id}/compose", post(handlers::compose::send))
        .route(
            "/accounts/{id}/compose/reply",
            post(handlers::compose::reply),
        )
        // Drafts
        .route("/accounts/{id}/drafts", get(handlers::drafts::list))
        .route(
            "/accounts/{id}/drafts/{draft_id}",
            get(handlers::drafts::show),
        )
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

    Router::new()
        .route("/", get(index_page))
        .route("/{account}/drafts/{draft_id}", get(index_page))
        .route("/accounts/{account}/drafts/{draft_id}", get(index_page))
        .route("/{account}/cockpit", get(index_page))
        .route("/accounts/{account}/cockpit", get(index_page))
        .route("/static/{*path}", get(static_asset))
        .nest("/api", api)
        .with_state(state)
}

// ── Background unsnooze sweep ────────────────────────────────────────

async fn run_unsnooze_sweep(state: &AppState) -> anyhow::Result<()> {
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let due = {
        let db = state.db.lock().await;
        db.list_snoozed_due(&now, None)
            .map_err(|e| anyhow::anyhow!("db error: {e}"))?
    };

    if due.is_empty() {
        return Ok(());
    }

    info!("unsnooze sweep: {} message(s) due", due.len());

    for msg in &due {
        // Try to get IMAP connection for this message's account
        let (client_arc, _creds) = match state.get_or_create_imap(&msg.account).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("unsnooze: IMAP connect failed for {}: {e}", msg.account);
                continue;
            }
        };
        let mut client = client_arc.lock().await;

        // Find the current UID (may have changed after move)
        let current_uid = if let Some(ref mid) = msg.message_id {
            let mid_clean = mid.trim_matches(|c| c == '<' || c == '>');
            match envelope_email_transport::imap::find_uid_by_message_id(
                &mut client,
                &msg.snoozed_folder,
                mid_clean,
            )
            .await
            {
                Ok(Some(uid)) => uid,
                _ => msg.uid,
            }
        } else {
            msg.uid
        };

        // Move back to original folder
        match envelope_email_transport::imap::move_message(
            &mut client,
            current_uid,
            &msg.snoozed_folder,
            &msg.original_folder,
        )
        .await
        {
            Ok(()) => {
                let db = state.db.lock().await;
                let _ = db.delete_snoozed(&msg.id);
                info!(
                    "unsnoozed UID {} back to {} ({})",
                    msg.uid, msg.original_folder, msg.account
                );
            }
            Err(e) => {
                tracing::warn!(
                    "unsnooze: move UID {} failed for {}: {e}",
                    msg.uid,
                    msg.account
                );
                state.evict_imap(&msg.account).await;
            }
        }
    }

    Ok(())
}

// ── Background scheduled send sweep ─────────────────────────────────

async fn run_scheduled_send_sweep(state: &AppState) -> anyhow::Result<()> {
    let due = {
        let db = state.db.lock().await;
        db.list_drafts_due_for_send()
            .map_err(|e| anyhow::anyhow!("db error: {e}"))?
    };

    if due.is_empty() {
        return Ok(());
    }

    info!("scheduled send sweep: {} draft(s) due", due.len());

    for draft in &due {
        // Resolve credentials for the draft's account
        let (client_arc, creds) = match state.get_or_create_imap(&draft.account_id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "scheduled send: failed to get credentials for {}: {e}",
                    draft.account_id
                );
                continue;
            }
        };
        // Drop the IMAP client lock — we only needed creds
        drop(client_arc);

        // Send via SMTP
        let subject = draft.subject.as_deref().unwrap_or("");
        match envelope_email_transport::SmtpSender::send_simple(
            &creds,
            &draft.to_addr,
            subject,
            draft.text_content.as_deref(),
            draft.html_content.as_deref(),
            draft.cc_addr.as_deref(),
            draft.bcc_addr.as_deref(),
            draft.reply_to.as_deref(),
        )
        .await
        {
            Ok(message_id) => {
                let db = state.db.lock().await;
                let _ = db.mark_draft_sent(&draft.id, Some(&message_id));
                info!(
                    "scheduled send: sent draft {} to {} ({})",
                    draft.id, draft.to_addr, message_id
                );
            }
            Err(e) => {
                tracing::warn!(
                    "scheduled send: SMTP failed for draft {} to {}: {e}",
                    draft.id,
                    draft.to_addr
                );
            }
        }
    }

    Ok(())
}

// ── Static asset serving ─────────────────────────────────────────────

async fn index_page() -> Response {
    match Assets::get_file("index.html") {
        Some(bytes) => {
            let html = String::from_utf8_lossy(&bytes)
                .replace("__ENVELOPE_VERSION__", env!("CARGO_PKG_VERSION"));
            Html(html).into_response()
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use envelope_email_store::{CredentialBackend, Database};
    use tower::ServiceExt;

    fn test_state() -> (AppState, String, String) {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Spain Expat', 'editor@spainexpat.com', 'spainexpat.com',
                         'smtp.spainexpat.com', 587, 'imap.spainexpat.com', 993, 'encrypted'),
                        ('acc2', 'Other', 'other@example.com', 'example.com',
                         'smtp.example.com', 587, 'imap.example.com', 993, 'encrypted')",
                [],
            )
            .unwrap();

        let draft = db
            .create_draft(
                "acc1",
                "tyler@example.com",
                Some("Review this Spain Expat reply"),
                Some("Looks ready to send."),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        let other_draft = db
            .create_draft(
                "acc2",
                "tyler@example.com",
                Some("Wrong account"),
                Some("This must not leak across accounts."),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        (
            AppState::new(db, CredentialBackend::File),
            draft.id,
            other_draft.id,
        )
    }

    #[tokio::test]
    async fn drafts_single_endpoint_resolves_account_by_username_and_is_account_scoped() {
        let (state, draft_id, other_draft_id) = test_state();
        let app = dashboard_router(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/accounts/editor@spainexpat.com/drafts/{draft_id}"
                    ))
                    .header("host", "localhost:1111")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["draft"]["id"], draft_id);
        assert_eq!(json["draft"]["account_id"], "acc1");
        assert_eq!(
            json["dashboard_path"],
            format!("/accounts/acc1/drafts/{draft_id}")
        );
        assert_eq!(
            json["dashboard_url"],
            format!("http://localhost:1111/accounts/acc1/drafts/{draft_id}")
        );
        assert_eq!(json["review_url"], json["dashboard_url"]);
        assert_eq!(json["metadata"]["dashboard_url"], json["dashboard_url"]);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/accounts/editor@spainexpat.com/drafts/{other_draft_id}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn assets_spa_fallback_serves_index_for_draft_deep_link_route() {
        let (state, draft_id, _) = test_state();
        let app = dashboard_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/editor@spainexpat.com/drafts/{draft_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("<title>Envelope</title>"));
        assert!(html.contains("/static/dashboard.js"));
    }
}
