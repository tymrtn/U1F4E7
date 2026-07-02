// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Envelope Email dashboard — localhost web UI and REST API.
//!
//! Mounts under `http://localhost:<port>/` (default 3141). Provides:
//! - HTML + static assets bundled via `rust-embed` from `static/`
//! - REST API under `/api/*` for accounts, folders, messages, compose,
//!   drafts, snooze, threads
//!
//! Binds `127.0.0.1` by default. The REST API mutates real mailboxes, so any
//! exposure beyond loopback — a non-loopback `--bind`, or a `tailscale serve`
//! front-end — must be authenticated. See [`auth`]. The dashboard refuses to
//! bind a non-loopback address unless an auth method is configured, and the
//! `/api` routes return `401` for unauthorized callers when auth is enforced.
//! The CORS allowlist is a browser-only defense and is *not* the access control.

pub mod assets;
pub mod auth;
pub mod handlers;
pub mod state;
mod ui_paths;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::Router;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use envelope_email_store::{CredentialBackend, Database};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::info;

use crate::assets::Assets;
use crate::auth::AuthConfig;
use crate::state::AppState;

/// Start the dashboard server on the given port.
///
/// Opens the default database, builds an [`AppState`] with an IMAP connection
/// pool, mounts the router, and blocks serving until shutdown.
pub async fn serve(port: u16) -> anyhow::Result<()> {
    serve_with_options(port, ServeOptions::default()).await
}

/// Start the dashboard server with a specific credential backend.
pub async fn serve_with_backend(port: u16, backend: CredentialBackend) -> anyhow::Result<()> {
    serve_with_backend_and_options(port, backend, ServeOptions::default()).await
}

/// Runtime options for the dashboard server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeOptions {
    /// Whether to run the periodic unsnooze and scheduled-send sweeps.
    ///
    /// Normal CLI/dashboard serving keeps this enabled. Diagnostic shells can
    /// disable it so merely opening the desktop app cannot move mail or send a
    /// scheduled draft.
    pub background_sweeps: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            background_sweeps: true,
        }
    }
}

impl ServeOptions {
    pub fn without_background_sweeps() -> Self {
        Self {
            background_sweeps: false,
        }
    }
}

/// Full configuration for [`serve_with_config`], the richest serve entrypoint.
pub struct ServeConfig {
    pub port: u16,
    /// Address to bind. Defaults to loopback; a non-loopback bind requires an
    /// enforced [`AuthConfig`] or the server refuses to start.
    pub bind: IpAddr,
    pub backend: CredentialBackend,
    pub options: ServeOptions,
    pub auth: AuthConfig,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            port: 3141,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            backend: CredentialBackend::File,
            options: ServeOptions::default(),
            auth: AuthConfig::disabled(),
        }
    }
}

/// Start the dashboard server with explicit runtime options.
pub async fn serve_with_options(port: u16, options: ServeOptions) -> anyhow::Result<()> {
    serve_with_backend_and_options(port, CredentialBackend::File, options).await
}

/// Start the dashboard server with a specific credential backend and options.
pub async fn serve_with_backend_and_options(
    port: u16,
    backend: CredentialBackend,
    options: ServeOptions,
) -> anyhow::Result<()> {
    serve_with_config(ServeConfig {
        port,
        backend,
        options,
        ..ServeConfig::default()
    })
    .await
}

/// Start the dashboard server with full configuration, including bind address
/// and authentication policy. Fails closed: a non-loopback bind with no auth
/// method configured is rejected before the listener opens.
pub async fn serve_with_config(cfg: ServeConfig) -> anyhow::Result<()> {
    let ServeConfig {
        port,
        bind,
        backend,
        options,
        auth,
    } = cfg;

    // `is_loopback()` returns false for IPv4-mapped IPv6 loopback
    // (`::ffff:127.0.0.1`), so such a bind is intentionally treated as
    // non-loopback and requires auth — the safe direction. Do not "fix" this to
    // treat mapped loopback as loopback; it would loosen the guard.
    if !bind.is_loopback() && !auth.is_enforced() {
        anyhow::bail!(
            "refusing to bind {bind}:{port} with no authentication. The dashboard \
             mutates real mailboxes; exposing it beyond loopback without a credential \
             would let any reachable host read and send mail. Set a token \
             (ENVELOPE_DASHBOARD_TOKEN or `envelope config set dashboard.auth_token <token>`), \
             or a Tailscale identity allowlist (dashboard.tailscale_allow), before \
             binding a non-loopback address. To keep it local, drop --bind (defaults to 127.0.0.1)."
        );
    }

    // Identity-only auth trusts the `Tailscale-User-Login` header, which any
    // process that can reach the bound port could forge. That is safe only when
    // the port is fronted by `tailscale serve` (loopback bind). On a broad bind
    // the header is forgeable by any reachable host — warn and recommend a token.
    if !bind.is_loopback() && auth.is_identity_only() {
        eprintln!(
            "warning: identity-only auth on a non-loopback bind ({bind}). The \
             Tailscale-User-Login header is forgeable by anything that can reach \
             this port. Prefer a bearer token (dashboard.auth_token) for broad \
             binds, and reserve the identity allowlist for a loopback bind fronted \
             by `tailscale serve`."
        );
    }

    let db = Database::open_default().map_err(|e| anyhow::anyhow!("{e}"))?;
    let state = AppState::new(db, backend).with_auth(auth);

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

    let addr = SocketAddr::from((bind, port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;

    let host_label = if bind.is_loopback() {
        "localhost".to_string()
    } else {
        bind.to_string()
    };
    info!(
        "dashboard listening on http://{host_label}:{port} (auth: {})",
        state.auth.mode_label()
    );
    println!("Envelope dashboard running at http://{host_label}:{port}");
    println!("Authentication: {}", state.auth.mode_label());
    if !bind.is_loopback() {
        println!(
            "Bound to a non-loopback address — every /api request must present a \
             valid credential."
        );
    }
    if options.background_sweeps {
        println!("Background unsnooze + scheduled-send sweep running every 60s");
        let ticker_state = state.clone();
        tokio::spawn(async move {
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
    } else {
        println!("Background unsnooze + scheduled-send sweeps disabled for diagnostic mode");
    }

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {e}"))
}

fn dashboard_router(state: AppState) -> Router {
    // Everything under here mutates or reads real mailbox data and is guarded by
    // the auth middleware when auth is enforced. `/api/health` is deliberately
    // kept OUT of this sub-router so an unauthenticated liveness probe still
    // works — it returns a minimal, path-free payload to unauthenticated callers
    // and the full drift-detection payload only to authorized ones.
    let protected = Router::new()
        // Accounts
        .route(
            "/accounts",
            get(handlers::accounts::list).post(handlers::accounts::create),
        )
        .route("/accounts/{id}", delete(handlers::accounts::delete))
        .route("/accounts/{id}/verify", post(handlers::accounts::verify))
        .route(
            "/accounts/{id}/setup-instructions",
            get(handlers::accounts::setup_instructions),
        )
        .route("/accounts/discover", post(handlers::accounts::discover))
        // Agent Cockpit
        .route("/cockpit", get(handlers::cockpit::get))
        .route(
            "/accounts/{id}/cockpit",
            get(handlers::cockpit::get_for_account),
        )
        // Folders
        .route("/accounts/{id}/folders", get(handlers::folders::list))
        // Messages
        .route("/messages/unified", get(handlers::messages::unified_inbox))
        .route(
            "/messages/unified/refresh",
            post(handlers::messages::refresh_unified_inbox),
        )
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
            "/accounts/{id}/rules/{rule_id}/preview",
            post(handlers::rules::preview),
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
        .route(
            "/accounts/{id}/drafts/{draft_id}/approve",
            post(handlers::drafts::approve),
        )
        .route(
            "/accounts/{id}/drafts/{draft_id}/edit",
            post(handlers::drafts::edit),
        )
        .route(
            "/accounts/{id}/drafts/{draft_id}/discard",
            post(handlers::drafts::discard),
        )
        .route(
            "/accounts/{id}/drafts/{draft_id}/block",
            post(handlers::drafts::block),
        )
        .route(
            "/accounts/{id}/drafts/{draft_id}/send",
            post(handlers::drafts::send),
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
        .route("/stats", get(handlers::stats::get))
        // Enforce auth on every protected route (no-op in open loopback mode).
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let api = Router::new()
        // Health / build identity (drift detection, issue #46). Unauthenticated
        // callers get a minimal liveness payload; authorized callers get paths.
        .route("/health", get(handlers::health::get))
        .merge(protected);

    Router::new()
        .route("/", get(index_page))
        // Frontend deep links emitted by CLI/MCP `ui` metadata. These must
        // serve the SPA shell, not 404, so links like
        // `/accounts/<id>/messages/<uid>?folder=INBOX` open cleanly. Legacy
        // back-compat paths without the `/accounts/` prefix are kept too.
        .route("/{account}/drafts/{draft_id}", get(index_page))
        .route("/accounts/{account}/drafts/{draft_id}", get(index_page))
        .route("/{account}/cockpit", get(index_page))
        .route("/accounts/{id}/cockpit", get(index_page))
        .route("/accounts/{id}/rules", get(index_page))
        .route("/accounts/{id}/messages/{uid}", get(index_page))
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

        // Rehydrate any attachment bytes snapshotted at schedule time. If the
        // stored payload is corrupt/undecodable, refuse to send (do not silently
        // deliver without the attachment); mark the draft blocked so the sweep
        // stops retrying and the failure is visible in scheduled-send status.
        let attachments = match decode_scheduled_attachments(&draft.attachments) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    "scheduled send: skipping draft {} — attachment decode failed: {e}",
                    draft.id
                );
                let db = state.db.lock().await;
                let _ =
                    db.update_draft_status(&draft.id, envelope_email_store::DraftStatus::Blocked);
                continue;
            }
        };

        // Send via SMTP — use the full send path so attachments and threading
        // headers are included. This is critical for queued replies: a cooldown
        // must not turn a contextual reply draft into an orphan `Re:` message.
        let subject = draft.subject.as_deref().unwrap_or("");
        let (thread_in_reply_to, thread_references) = scheduled_threading(draft);
        let thread_references_opt = if thread_references.is_empty() {
            None
        } else {
            Some(thread_references.as_slice())
        };

        // ── Governor gate (fail-closed before any real SMTP) ──
        //
        // The scheduled-send sweep is the one place that actually transmits
        // queued mail, so it must run the Governor gate. When Governor is
        // required and missing/errors/denies/reviews, the send is refused and
        // the draft stays queued (the sweep retries next cycle).
        let gov_outcome = run_governor_gate(state, draft, &creds, subject, &attachments).await;
        if !gov_outcome.allowed {
            // A durable Governor verdict (review/deny/block) must not be retried
            // on every sweep. Leaving the draft in `draft` status with a past
            // `send_after` would re-select it via `list_drafts_due_for_send` each
            // cycle, re-running the gate and emitting a fresh
            // `send_governor.blocked` event forever. Instead, transition the
            // draft to `pending_review`: the due query only selects
            // `status = 'draft'`, so the draft durably drops out of the sweep
            // while remaining preserved, editable, and re-sendable by explicit
            // human action. Transient gate failures (Governor unavailable) are
            // left queued so a later sweep can retry once Governor is reachable.
            let pause_for_review = should_pause_for_review(&gov_outcome);
            tracing::warn!(
                "scheduled send: governor blocked draft {} ({}){}",
                draft.id,
                gov_outcome
                    .block_code
                    .clone()
                    .unwrap_or_else(|| "governor_blocked".to_string()),
                if pause_for_review {
                    " — moving to pending_review"
                } else {
                    " — leaving queued for retry"
                }
            );
            if pause_for_review {
                let db = state.db.lock().await;
                let _ = db.update_draft_status(
                    &draft.id,
                    envelope_email_store::DraftStatus::PendingReview,
                );
            }
            continue;
        }

        match envelope_email_transport::SmtpSender::send(
            &creds,
            &draft.to_addr,
            subject,
            draft.text_content.as_deref(),
            draft.html_content.as_deref(),
            None, // from override — not persisted for scheduled sends
            draft.cc_addr.as_deref(),
            draft.bcc_addr.as_deref(),
            draft.reply_to.as_deref(),
            thread_in_reply_to.as_deref(),
            thread_references_opt,
            &attachments,
        )
        .await
        {
            Ok(message_id) => {
                let db = state.db.lock().await;
                let _ = db.mark_draft_sent(&draft.id, Some(&message_id));
                info!(
                    "scheduled send: sent draft {} (recipient_count={}, message_id={})",
                    draft.id,
                    recipient_count_for_log(
                        &draft.to_addr,
                        draft.cc_addr.as_deref(),
                        draft.bcc_addr.as_deref()
                    ),
                    message_id
                );
            }
            Err(e) => {
                tracing::warn!(
                    "scheduled send: SMTP failed for draft {} (recipient_count={}): {e}",
                    draft.id,
                    recipient_count_for_log(
                        &draft.to_addr,
                        draft.cc_addr.as_deref(),
                        draft.bcc_addr.as_deref()
                    )
                );
            }
        }
    }

    Ok(())
}

fn recipient_count_for_log(to: &str, cc: Option<&str>, bcc: Option<&str>) -> usize {
    [Some(to), cc, bcc]
        .into_iter()
        .flatten()
        .flat_map(|value| value.split(','))
        .filter(|token| token.contains('@'))
        .count()
}

fn scheduled_threading(draft: &envelope_email_store::Draft) -> (Option<String>, Vec<String>) {
    let meta = draft.metadata.as_ref();
    let meta_in_reply_to = meta
        .and_then(|m| m.get("in_reply_to"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let references = meta
        .and_then(|m| m.get("references"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    (draft.in_reply_to.clone().or(meta_in_reply_to), references)
}

/// Decide whether a blocked scheduled draft should be durably paused into
/// `pending_review` versus left queued for a later retry.
///
/// A real Governor verdict (review/deny/block, surfaced as the
/// `governor_blocked` block code) is durable: the answer will not change on the
/// next sweep, so the draft must stop retrying and move to `pending_review` for
/// explicit human action. A transient gate failure (Governor unavailable /
/// unparseable, surfaced as `governor_unavailable`) is left queued so a later
/// sweep can retry once Governor is reachable again.
fn should_pause_for_review(outcome: &envelope_email_transport::outbound::GovernorOutcome) -> bool {
    !outcome.allowed && outcome.block_code.as_deref() == Some("governor_blocked")
}

/// Derive the final attributed context for a due scheduled draft from what will
/// actually be transmitted. Pure and side-effect free: recipients and threading
/// come from the persisted draft, attachment sensitivity is classified from the
/// rehydrated snapshot's filenames (class only — bytes are never inspected for
/// scoring). This is the authoritative context the sweep gates on.
fn scheduled_send_context(
    draft: &envelope_email_store::Draft,
    account_domain: Option<String>,
    attachments: &[envelope_email_transport::smtp::Attachment],
) -> envelope_email_transport::attribution::AttributedSendContext {
    use envelope_email_transport::attribution::{
        AttributedSendContext, classify_sensitive_attachment, collect_recipient_domains,
    };
    let summary = collect_recipient_domains(
        &draft.to_addr,
        draft.cc_addr.as_deref(),
        draft.bcc_addr.as_deref(),
    );
    let sensitive_attachment = attachments
        .iter()
        .any(|a| classify_sensitive_attachment(&a.filename, &a.content_type));
    AttributedSendContext {
        account_domain,
        recipient_domains: summary.domains,
        recipient_count: summary.count,
        is_reply: draft.in_reply_to.is_some() || scheduled_threading(draft).0.is_some(),
        has_bcc: summary.has_bcc,
        attachment_count: attachments.len(),
        sensitive_attachment,
        ..Default::default()
    }
}

/// Run the Governor gate for a due scheduled draft and record a sanitized audit
/// event. Returns the outcome; the sweep must refuse SMTP unless allowed.
async fn run_governor_gate(
    state: &AppState,
    draft: &envelope_email_store::Draft,
    creds: &envelope_email_store::models::AccountWithCredentials,
    subject: &str,
    attachments: &[envelope_email_transport::smtp::Attachment],
) -> envelope_email_transport::outbound::GovernorOutcome {
    use envelope_email_transport::outbound::{GovernorConfig, GovernorRequest, SendSurface, gate};

    let account_domain = creds
        .account
        .username
        .rsplit_once('@')
        .map(|(_, d)| d.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty());

    let attachment_sizes: Vec<(String, u64)> = attachments
        .iter()
        .map(|a| (a.content_type.clone(), a.data.len() as u64))
        .collect();

    // Re-derive the FINAL attributed context from the persisted draft just
    // before SMTP. This is the authoritative gate: recipients, attachments, and
    // threading are read from what will actually be transmitted.
    let ctx = scheduled_send_context(draft, account_domain, attachments);
    let req = GovernorRequest::from_context(
        &draft.account_id,
        subject,
        SendSurface::Scheduled,
        Some(&draft.id),
        &attachment_sizes,
        &ctx,
    );

    let config = GovernorConfig::from_env();
    let outcome = gate(&config, &req);

    // Record a sanitized audit event (no bodies, no full addresses, no bytes).
    let event_type = if outcome.allowed {
        "send_governor.allowed"
    } else {
        "send_governor.blocked"
    };
    let payload = serde_json::json!({
        "request": req.audit_payload(),
        "outcome": outcome.audit_json(),
    });
    let event = envelope_email_store::Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: draft.account_id.clone(),
        event_type: event_type.to_string(),
        folder: "policy".to_string(),
        uid: None,
        message_id: None,
        from_addr: None,
        subject: None,
        snippet: None,
        payload: Some(payload.to_string()),
        idempotency_key: None,
        secure_pending: false,
        acked_at: Some(chrono::Utc::now().to_rfc3339()),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    {
        let db = state.db.lock().await;
        let _ = db.insert_event(&event);
    }

    outcome
}

/// Decode draft attachment JSON entries (as snapshotted at schedule time) back
/// into transport `Attachment`s with their original bytes.
///
/// Entries are expected to carry `filename`, `content_type`, and a base64
/// `data_base64` payload. Returns an error if any entry is missing its byte
/// payload or fails to decode, so the caller can refuse to send rather than
/// silently dropping the attachment.
fn decode_scheduled_attachments(
    attachments: &[serde_json::Value],
) -> anyhow::Result<Vec<envelope_email_transport::smtp::Attachment>> {
    use base64::Engine as _;
    let mut out = Vec::with_capacity(attachments.len());
    for entry in attachments {
        let filename = entry
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("attachment")
            .to_string();
        let content_type = entry
            .get("content_type")
            .and_then(|v| v.as_str())
            .unwrap_or("application/octet-stream")
            .to_string();
        let data_b64 = entry
            .get("data_base64")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("attachment '{filename}' has no data_base64 payload"))?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| anyhow::anyhow!("attachment '{filename}' base64 decode failed: {e}"))?;
        out.push(envelope_email_transport::smtp::Attachment {
            filename,
            content_type,
            data,
        });
    }
    Ok(out)
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

    #[test]
    fn scheduled_threading_preserves_contextual_reply_headers() {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Agent', 'agent@example.com', 'example.com',
                         'smtp.example.com', 587, 'imap.example.com', 993, 'encrypted')",
                [],
            )
            .unwrap();
        let draft = db
            .create_draft(
                "acc1",
                "sender@example.net",
                Some("Re: Threaded"),
                Some("reply body"),
                None,
                Some("parent@example.net"),
                None,
                None,
                Some("mcp"),
            )
            .unwrap();
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({
                "draft_kind": "reply",
                "in_reply_to": "metadata-parent@example.net",
                "references": ["root@example.net", "parent@example.net"]
            }),
        )
        .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();

        let (in_reply_to, references) = scheduled_threading(&fetched);
        assert_eq!(in_reply_to.as_deref(), Some("parent@example.net"));
        assert_eq!(references, vec!["root@example.net", "parent@example.net"]);
    }

    fn governor_outcome(
        decision: &str,
        block_code: Option<&str>,
    ) -> envelope_email_transport::outbound::GovernorOutcome {
        envelope_email_transport::outbound::GovernorOutcome {
            allowed: false,
            mode: envelope_email_transport::outbound::GovernorMode::Required,
            decision: decision.to_string(),
            state: None,
            score: None,
            review_ticket_id: None,
            block_code: block_code.map(str::to_string),
            block_reason: Some("blocked".to_string()),
        }
    }

    #[test]
    fn review_and_deny_verdicts_pause_for_review_but_unavailable_retries() {
        // Durable Governor verdicts (block code `governor_blocked`) pause the draft.
        for decision in ["review", "deny", "block"] {
            assert!(
                should_pause_for_review(&governor_outcome(decision, Some("governor_blocked"))),
                "{decision} should pause for review"
            );
        }
        // Transient gate failure stays queued for a later retry.
        assert!(!should_pause_for_review(&governor_outcome(
            "unavailable",
            Some("governor_unavailable")
        )));
        // An allowed outcome never pauses.
        let mut allowed = governor_outcome("allow", None);
        allowed.allowed = true;
        assert!(!should_pause_for_review(&allowed));
    }

    #[test]
    fn review_required_scheduled_draft_drops_out_of_sweep_yet_stays_reviewable() {
        use envelope_email_store::DraftStatus;

        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Agent', 'agent@example.com', 'example.com',
                         'smtp.example.com', 587, 'imap.example.com', 993, 'encrypted')",
                [],
            )
            .unwrap();
        let draft = db
            .create_draft(
                "acc1",
                "recipient@example.net",
                Some("Scheduled note"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        // Schedule it in the past so it is due now.
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();

        // Before the Governor pause, the sweep would pick this draft up.
        let due_before = db.list_drafts_due_for_send().unwrap();
        assert!(due_before.iter().any(|d| d.id == draft.id));

        // Governor classifies the send as review-required: pause it durably.
        assert!(should_pause_for_review(&governor_outcome(
            "review",
            Some("governor_blocked")
        )));
        db.update_draft_status(&draft.id, DraftStatus::PendingReview)
            .unwrap();

        // It is no longer due — the sweep will not retry it next cycle.
        let due_after = db.list_drafts_due_for_send().unwrap();
        assert!(
            !due_after.iter().any(|d| d.id == draft.id),
            "paused draft must not be re-selected by the scheduled-send sweep"
        );

        // But the draft is preserved, still pending review, and re-sendable by
        // explicit human action (not discarded, still editable).
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(fetched.status, DraftStatus::PendingReview);
        assert!(fetched.status.is_editable());
        assert_eq!(fetched.send_after.as_deref(), Some("2000-01-01T00:00:00Z"));
    }

    #[test]
    fn scheduled_send_context_re_derives_final_attributes_from_persisted_draft() {
        use envelope_email_transport::smtp::Attachment;

        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Agent', 'agent@martin.fm', 'martin.fm',
                         'smtp.example.com', 587, 'imap.example.com', 993, 'encrypted')",
                [],
            )
            .unwrap();

        // A queued reply to an external freemail recipient carrying a contract.
        let draft = db
            .create_draft(
                "acc1",
                "counterparty@gmail.com",
                Some("Re: Services agreement"),
                Some("body"),
                None,
                Some("parent@martin.fm"),
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        let attachments = vec![Attachment {
            filename: "Master-Services-Agreement.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            data: b"%PDF-1.4 fake".to_vec(),
        }];

        // The authoritative gate re-derives from what will actually be sent.
        let ctx = scheduled_send_context(&draft, Some("martin.fm".to_string()), &attachments);
        let attrs = ctx.to_governor_attrs();

        assert!(attrs.contains(&"reply_to_thread"), "{attrs:?}");
        assert!(attrs.contains(&"freemail_domain"), "{attrs:?}");
        assert!(attrs.contains(&"has_attachment"), "{attrs:?}");
        assert!(attrs.contains(&"sensitive_attachment"), "{attrs:?}");
        // External recipient — never internal.
        assert!(!attrs.contains(&"internal_domain"), "{attrs:?}");
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

    // ── Dashboard authentication (tailnet exposure guard) ────────────────

    async fn get_api(app: &Router, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder().uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, body)
    }

    #[tokio::test]
    async fn open_mode_allows_protected_api_without_credentials() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);
        let (status, _) = get_api(&app, "/api/accounts", &[]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn token_mode_rejects_protected_api_without_valid_bearer() {
        let (state, _, _) = test_state();
        let app =
            dashboard_router(state.with_auth(AuthConfig::from_parts(Some("t0ken".into()), [])));

        let (unauth, body) = get_api(&app, "/api/accounts", &[]).await;
        assert_eq!(unauth, StatusCode::UNAUTHORIZED, "no credential → 401");
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "dashboard_auth_required");

        let (wrong, _) = get_api(&app, "/api/accounts", &[("authorization", "Bearer nope")]).await;
        assert_eq!(wrong, StatusCode::UNAUTHORIZED, "wrong token → 401");

        let (ok, _) = get_api(&app, "/api/accounts", &[("authorization", "Bearer t0ken")]).await;
        assert_eq!(ok, StatusCode::OK, "correct bearer → 200");

        let (ok2, _) = get_api(&app, "/api/accounts", &[("x-envelope-token", "t0ken")]).await;
        assert_eq!(ok2, StatusCode::OK, "fallback header → 200");
    }

    #[tokio::test]
    async fn tailscale_identity_allowlist_gates_protected_api() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state.with_auth(AuthConfig::from_parts(
            None,
            ["skippy@tail.ts.net".to_string()],
        )));

        let (denied, _) = get_api(
            &app,
            "/api/accounts",
            &[("tailscale-user-login", "intruder@tail.ts.net")],
        )
        .await;
        assert_eq!(denied, StatusCode::UNAUTHORIZED);

        let (allowed, _) = get_api(
            &app,
            "/api/accounts",
            &[("tailscale-user-login", "skippy@tail.ts.net")],
        )
        .await;
        assert_eq!(allowed, StatusCode::OK);
    }

    #[tokio::test]
    async fn health_is_reachable_but_path_free_when_unauthenticated() {
        let (state, _, _) = test_state();
        let app =
            dashboard_router(state.with_auth(AuthConfig::from_parts(Some("t0ken".into()), [])));

        // Unauthenticated: 200 liveness, but no filesystem paths leaked.
        let (status, body) = get_api(&app, "/api/health", &[]).await;
        assert_eq!(status, StatusCode::OK, "health stays reachable for probes");
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert!(json["version"].is_string());
        assert!(json["database_path"].is_null(), "must not leak db path");
        assert!(json["binary_path"].is_null(), "must not leak binary path");

        // Authorized: full drift-detection payload.
        let (status, body) =
            get_api(&app, "/api/health", &[("authorization", "Bearer t0ken")]).await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["database_path"].is_string(), "authorized sees paths");
    }

    #[tokio::test]
    async fn open_mode_health_returns_full_payload() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);
        let (status, body) = get_api(&app, "/api/health", &[]).await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["database_path"].is_string(),
            "local doctor drift detection unchanged in open mode"
        );
    }

    #[tokio::test]
    async fn setup_instructions_endpoint_returns_non_secret_fields() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/accounts/editor@spainexpat.com/setup-instructions")
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
        assert_eq!(json["email"], "editor@spainexpat.com");
        assert_eq!(json["imap"]["host"], "imap.spainexpat.com");
        assert_eq!(json["imap"]["port"], 993);
        assert_eq!(json["imap"]["security"], "SSL/TLS");
        assert_eq!(json["smtp"]["host"], "smtp.spainexpat.com");
        assert_eq!(json["smtp"]["port"], 587);
        assert_eq!(json["smtp"]["security"], "STARTTLS");
        // The encrypted password must never leak into setup output.
        let serialized = serde_json::to_string(&json).unwrap();
        assert!(!serialized.contains("encrypted"));
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

    #[tokio::test]
    async fn spa_fallback_serves_index_for_message_and_cockpit_deep_links() {
        // Issue #47: URLs emitted by CLI/MCP `ui` metadata
        // (`/accounts/<id>/messages/<uid>?folder=INBOX`, `/accounts/<id>/cockpit`)
        // must resolve to the SPA shell instead of 404 so the links are clickable.
        let (state, _, _) = test_state();
        let app = dashboard_router(state);

        for uri in [
            "/accounts/acc1/messages/57?folder=INBOX",
            "/accounts/acc1/cockpit",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "deep link {uri} should serve the SPA shell, not 404"
            );
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let html = String::from_utf8(body.to_vec()).unwrap();
            assert!(
                html.contains("<title>Envelope</title>") && html.contains("/static/dashboard.js"),
                "deep link {uri} should return the dashboard SPA index"
            );
        }
    }

    #[test]
    fn decode_scheduled_attachments_round_trips_bytes() {
        let attachments = vec![
            serde_json::json!({
                "filename": "packet.txt",
                "content_type": "text/plain",
                "size": 5,
                "data_base64": "aGVsbG8=",
            }),
            serde_json::json!({
                "filename": "r.bin",
                "content_type": "application/octet-stream",
                "size": 3,
                "data_base64": "Zm9v",
            }),
        ];
        let decoded = decode_scheduled_attachments(&attachments).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].filename, "packet.txt");
        assert_eq!(decoded[0].data, b"hello");
        assert_eq!(decoded[1].data, b"foo");
    }

    #[test]
    fn decode_scheduled_attachments_errors_on_missing_payload() {
        let attachments = vec![serde_json::json!({
            "filename": "packet.txt",
            "content_type": "text/plain",
            "size": 5,
        })];
        let err = decode_scheduled_attachments(&attachments).unwrap_err();
        assert!(err.to_string().contains("no data_base64"));
    }

    #[test]
    fn decode_scheduled_attachments_errors_on_bad_base64() {
        let attachments = vec![serde_json::json!({
            "filename": "packet.txt",
            "content_type": "text/plain",
            "data_base64": "!!!not-base64!!!",
        })];
        let err = decode_scheduled_attachments(&attachments).unwrap_err();
        assert!(err.to_string().contains("base64 decode failed"));
    }

    #[test]
    fn decode_scheduled_attachments_empty_is_empty() {
        assert!(decode_scheduled_attachments(&[]).unwrap().is_empty());
    }

    #[test]
    fn serve_options_keep_background_sweeps_enabled_by_default() {
        assert!(ServeOptions::default().background_sweeps);
    }

    #[test]
    fn diagnostic_serve_options_disable_background_sweeps() {
        assert!(!ServeOptions::without_background_sweeps().background_sweeps);
    }
}
