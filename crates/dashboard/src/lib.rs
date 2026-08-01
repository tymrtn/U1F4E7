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
pub mod csrf;
pub mod events;
pub mod handlers;
pub mod state;
pub mod timefmt;
mod ui_paths;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use envelope_email_store::{CredentialBackend, Database};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::info;

use crate::assets::WebAssets;
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
/// and authentication policy. Fails closed: a non-loopback bind without a
/// bearer token is rejected before the listener opens.
pub async fn serve_with_config(cfg: ServeConfig) -> anyhow::Result<()> {
    let ServeConfig {
        port,
        bind,
        backend,
        options,
        auth,
    } = cfg;

    validate_dashboard_bind(bind, port, &auth)?;

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
        println!("Background unsnooze + scheduled-send + event-delivery sweep running every 60s");
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

        // The durable webhook delivery executor interleaves DB reads/writes with
        // HTTP awaits and holds a non-Send rusqlite handle across those awaits,
        // so it cannot run on the multi-threaded runtime's `tokio::spawn` (which
        // requires Send). Give it its own OS thread with a current-thread runtime
        // and its own DB connection (WAL makes the second connection safe against
        // the shared state DB). This keeps the existing sweeps untouched.
        spawn_event_delivery_sweeper();
    } else {
        println!("Background unsnooze + scheduled-send sweeps disabled for diagnostic mode");
    }

    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {e}"))
}

/// Validate the dashboard exposure boundary before opening a listener.
///
/// `is_loopback()` returns false for IPv4-mapped IPv6 loopback
/// (`::ffff:127.0.0.1`), so such a bind is intentionally treated as
/// non-loopback and requires a bearer token. Do not loosen this guard: a
/// Tailscale identity header is only trustworthy when `tailscale serve` adds
/// it to a loopback listener.
fn validate_dashboard_bind(bind: IpAddr, port: u16, auth: &AuthConfig) -> anyhow::Result<()> {
    if bind.is_loopback() {
        return Ok(());
    }

    if !auth.is_enforced() {
        anyhow::bail!(
            "refusing to bind {bind}:{port} with no authentication. The dashboard \
             mutates real mailboxes; exposing it beyond loopback without a credential \
             would let any reachable host read and send mail. Set a bearer token \
             (ENVELOPE_DASHBOARD_TOKEN or `envelope config set dashboard.auth_token <token>`) \
             before binding a non-loopback address. To keep it local, drop --bind \
             (defaults to 127.0.0.1)."
        );
    }

    if auth.has_tailscale_identity_allowlist() {
        anyhow::bail!(
            "refusing to bind {bind}:{port} with a Tailscale identity allowlist. The \
             Tailscale-User-Login header is forgeable by anything that can reach a broad \
             listener, even when a bearer token is also configured. Keep the dashboard on \
             loopback behind `tailscale serve`, or remove dashboard.tailscale_allow and use \
             a bearer token (ENVELOPE_DASHBOARD_TOKEN or `envelope config set \
             dashboard.auth_token <token>`) for a non-loopback bind."
        );
    }

    Ok(())
}

/// Build the dashboard router (HTML shell, static assets, and the `/api`
/// surface) for a given [`AppState`]. Public for integration tests; production
/// serving goes through [`serve_with_config`], which also attaches CORS.
#[doc(hidden)]
pub fn dashboard_router(state: AppState) -> Router {
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
        // Per-agent attribution feed + approval queue (read-only aggregate).
        .route("/agents", get(handlers::agents::get))
        // Scheduled sends + Governor verdict visibility (read-only aggregate).
        .route("/scheduled", get(handlers::scheduled::get))
        .route(
            "/accounts/{id}/scheduled",
            get(handlers::scheduled::get_for_account),
        )
        // Watch + delivery health browser (read-only aggregate).
        .route("/watches", get(handlers::watches::get))
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
        // Rules — read
        .route(
            "/accounts/{id}/rules",
            get(handlers::rules::list).post(handlers::rules::create),
        )
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
        // Rules — write
        .route(
            "/accounts/{id}/rules/{rule_id}",
            put(handlers::rules::update).delete(handlers::rules::destroy),
        )
        .route(
            "/accounts/{id}/rules/{rule_id}/enable",
            post(handlers::rules::enable),
        )
        .route(
            "/accounts/{id}/rules/{rule_id}/disable",
            post(handlers::rules::disable),
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
            "/accounts/{id}/drafts/by-imap-uid/{imap_uid}",
            get(handlers::drafts::show_by_imap_uid),
        )
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
        // Real-time event stream (SSE). GET → CSRF-exempt; auth applies. The
        // browser `EventSource` rides the cookie/identity credential; bearer-only
        // clients pass `?access_token=`.
        .route("/events/stream", get(handlers::events_stream::stream))
        // CSRF token mint. Inside the protected router so it shares the auth
        // gate, but GET is never CSRF-checked so it is always reachable to the
        // authorized frontend.
        .route("/csrf", get(csrf::issue))
        // CSRF enforcement on mutating methods. Layered BEFORE `require_auth`
        // below so that, at request time, `require_auth` is the OUTER layer:
        // it runs first, authorizes, and records `BearerAuthenticated`, then
        // this inner CSRF layer reads that extension to exempt bearer clients.
        .route_layer(axum::middleware::from_fn(csrf::require_csrf))
        // Enforce auth on every protected route (no-op in open loopback mode).
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let api = Router::new()
        // Health / build identity (drift detection, issue #46). Unauthenticated
        // callers get a minimal liveness payload; authorized callers get paths.
        .route("/health", get(handlers::health::get))
        .merge(protected)
        // Unmatched `/api/*` paths return JSON 404, not the SPA shell — the
        // root fallback below would otherwise serve HTML for a bad API call.
        .fallback(api_not_found);

    // ── Envelope v2 webmail (SvelteKit SPA, adapter-static) ──
    // As of 1.0.0 the v2 webmail IS the dashboard: it serves at `/`, and the
    // root fallback returns embedded `web/build/` assets or the SPA shell for
    // client-side routes (`/cockpit`, `/rules`, `/mail/...`) built with
    // `paths.base = ''`. The old v1 static dashboard and its `/v2` mount are
    // gone. CLI/MCP `ui` deep links resolve through the same SPA shell.
    Router::new()
        .route("/", get(spa_shell))
        .nest("/api", api)
        .fallback(spa_fallback)
        .with_state(state)
}

// ── Background unsnooze sweep ────────────────────────────────────────

async fn run_unsnooze_sweep(state: &AppState) -> anyhow::Result<()> {
    // Snooze `return_at` rows are stored in UTC (naive or `Z`); comparing them
    // against local wall-clock time skews every unsnooze by the UTC offset.
    let now = timefmt::utc_now_string();
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
                {
                    let db = state.db.lock().await;
                    let _ = db.delete_snoozed(&msg.id);
                }
                info!(
                    "unsnoozed UID {} back to {} ({})",
                    msg.uid, msg.original_folder, msg.account
                );
                state
                    .events
                    .publish(crate::events::DashboardEvent::Unsnoozed {
                        account_id: msg.account.clone(),
                        original_folder: msg.original_folder.clone(),
                    });
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

    for scanned in &due {
        // ── Atomic claim (id + revision + status) BEFORE any await ──
        //
        // A single CAS UPDATE moves the row `draft` → `sending`. Losing the
        // claim means another sweeper took it, a concurrent edit bumped the
        // revision, or an operator blocked/discarded it after the due scan —
        // in every case this sweeper must not transmit its (stale) snapshot.
        // While claimed the row is out of the due query, so a crash or later
        // DB failure can strand it as `sending` but never re-send it.
        let claimed = {
            let db = state.db.lock().await;
            db.claim_draft_for_sending(&scanned.id, scanned.revision)
        };
        let lease = match claimed {
            Ok(Some(token)) => token,
            Ok(None) => {
                info!(
                    "scheduled send: draft {} not claimed (concurrent claim, edit, or \
                     state change) — skipping this sweep",
                    scanned.id
                );
                continue;
            }
            Err(e) => {
                tracing::warn!("scheduled send: claim failed for draft {}: {e}", scanned.id);
                continue;
            }
        };

        // Reload the claimed row: the authoritative snapshot for attribution
        // and SMTP is what was claimed, not the pre-claim scan.
        let draft = {
            let db = state.db.lock().await;
            db.get_draft(&scanned.id)
        };
        let draft = match draft {
            Ok(Some(d)) => d,
            Ok(None) | Err(_) => {
                tracing::warn!(
                    "scheduled send: claimed draft {} could not be reloaded — releasing \
                     claim for retry",
                    scanned.id
                );
                release_claim(
                    state,
                    &scanned.id,
                    &lease,
                    envelope_email_store::DraftStatus::Draft,
                )
                .await;
                continue;
            }
        };
        let draft = &draft;

        // Resolve credentials for the draft's account.
        let (client_arc, creds) = match state.get_or_create_imap(&draft.account_id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "scheduled send: failed to get credentials for {}: {e} — releasing \
                     claim for retry",
                    draft.account_id
                );
                release_claim(
                    state,
                    &draft.id,
                    &lease,
                    envelope_email_store::DraftStatus::Draft,
                )
                .await;
                continue;
            }
        };
        // Drop the IMAP client lock — we only needed creds
        drop(client_arc);

        // Rehydrate any attachment bytes snapshotted at schedule time. If the
        // stored payload is corrupt/undecodable, refuse to send (do not silently
        // deliver without the attachment); park the draft blocked so the sweep
        // stops retrying and the failure is visible in scheduled-send status.
        let attachments = match decode_scheduled_attachments(&draft.attachments) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    "scheduled send: skipping draft {} — attachment decode failed: {e}",
                    draft.id
                );
                release_claim(
                    state,
                    &draft.id,
                    &lease,
                    envelope_email_store::DraftStatus::Blocked,
                )
                .await;
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
        // queued mail, so it must run the Governor gate against the reloaded,
        // claimed row. When Governor is required and missing/errors/denies/
        // reviews, the send is refused and the claim is released per reason.
        let gov_outcome = run_governor_gate(state, draft, &creds, subject, &attachments).await;
        if !gov_outcome.allowed {
            // A durable Governor verdict (review/deny/block) must not be retried
            // on every sweep: release the claim into `pending_review`, dropping
            // the draft out of the due query while it stays preserved, editable,
            // and re-sendable by explicit human action. Transient gate failures
            // (Governor unavailable) release back to `draft` so a later sweep
            // retries once Governor is reachable.
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
                    " — releasing claim for retry"
                }
            );
            release_claim(
                state,
                &draft.id,
                &lease,
                if pause_for_review {
                    envelope_email_store::DraftStatus::PendingReview
                } else {
                    envelope_email_store::DraftStatus::Draft
                },
            )
            .await;
            // Metadata-level send status: decision + optional block code only.
            // No recipients, subject, or body ever cross this channel.
            state
                .events
                .publish(crate::events::DashboardEvent::SendStatus {
                    account_id: draft.account_id.clone(),
                    draft_id: draft.id.clone(),
                    outcome: if pause_for_review {
                        "blocked"
                    } else {
                        "deferred"
                    },
                    governor_decision: Some(gov_outcome.decision.clone()),
                    governor_block_code: gov_outcome.block_code.clone(),
                });
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
                let persistence = {
                    let db = state.db.lock().await;
                    persist_sent_state(&db, &draft.id, &lease, &message_id)
                };
                // Honest logging: an unrecorded persistence outcome is not a
                // durable success and must not read like one (the persistence
                // failure itself was already logged at error level).
                match persistence {
                    SentPersistence::Recorded => info!(
                        "scheduled send: sent draft {} (recipient_count={}, message_id={})",
                        draft.id,
                        recipient_count_for_log(
                            &draft.to_addr,
                            draft.cc_addr.as_deref(),
                            draft.bcc_addr.as_deref()
                        ),
                        message_id
                    ),
                    SentPersistence::Unrecorded { parked } => tracing::warn!(
                        "scheduled send: draft {} transmitted (message_id={}) but sent \
                         state is UNRECORDED (parked={parked}) — not a durable success",
                        draft.id,
                        message_id
                    ),
                }
                // The original server-side draft copy is now stale. Clean it up
                // strictly AFTER SMTP acceptance AND durable sent-state
                // persistence — if the sent state did not persist, the local
                // draft is the only record of what happened and the provider
                // copy must be left alone. Identity needs only the exact
                // detected folder + persisted Message-ID (a stored UID is not
                // required and never trusted).
                if persistence == SentPersistence::Recorded {
                    cleanup_provider_draft_after_send(state, draft).await;
                }
                state
                    .events
                    .publish(crate::events::DashboardEvent::SendStatus {
                        account_id: draft.account_id.clone(),
                        draft_id: draft.id.clone(),
                        // Never report durable success when the sent state did
                        // not persist — the SMTP transmission happened, but
                        // Envelope's record of it is incomplete.
                        outcome: if persistence == SentPersistence::Recorded {
                            "sent"
                        } else {
                            "sent_unrecorded"
                        },
                        governor_decision: Some(gov_outcome.decision.clone()),
                        governor_block_code: None,
                    });
            }
            Err(e) => {
                tracing::warn!(
                    "scheduled send: SMTP result is inconclusive for draft {} \
                     (recipient_count={}): {e} — parking as delivery_uncertain to \
                     prevent an automatic duplicate",
                    draft.id,
                    recipient_count_for_log(
                        &draft.to_addr,
                        draft.cc_addr.as_deref(),
                        draft.bcc_addr.as_deref()
                    )
                );
                // SMTP errors can occur after the server accepts DATA but before
                // the client receives its final acknowledgement. No error variant
                // proves non-delivery, so retries would risk a duplicate message.
                // Keep the draft terminal until an operator reconciles delivery.
                let db = state.db.lock().await;
                park_delivery_uncertain(&db, &draft.id, &lease, "an inconclusive SMTP result");
                state
                    .events
                    .publish(crate::events::DashboardEvent::SendStatus {
                        account_id: draft.account_id.clone(),
                        draft_id: draft.id.clone(),
                        outcome: "delivery_uncertain",
                        governor_decision: Some(gov_outcome.decision.clone()),
                        governor_block_code: None,
                    });
            }
        }
    }

    Ok(())
}

/// Release a sweep claim into `to`, logging (but never panicking on) failure.
/// Only ever called before SMTP acceptance. A transmitted or inconclusive
/// delivery leaves the claim through `park_delivery_uncertain` or
/// `mark_draft_sent`. If the release fails, the row stays `sending`: stranded
/// but inert, and never re-selected for a duplicate transmission.
async fn release_claim(
    state: &AppState,
    draft_id: &str,
    lease: &str,
    to: envelope_email_store::DraftStatus,
) {
    let db = state.db.lock().await;
    match db.release_sending_draft(draft_id, lease, to.clone()) {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            "scheduled send: claim release for draft {draft_id} matched no `sending` row \
             (already transitioned?)"
        ),
        Err(e) => tracing::error!(
            "scheduled send: claim release for draft {draft_id} → {} failed: {e} — the \
             draft stays parked in `sending` (inert, never re-sent) until repaired",
            to.as_str()
        ),
    }
}

// ── Background event-delivery sweep ─────────────────────────────────

/// Spawn the dedicated OS thread that runs the durable webhook delivery executor
/// every 60s on its own current-thread runtime with its own DB connection.
fn spawn_event_delivery_sweeper() {
    std::thread::Builder::new()
        .name("envelope-event-delivery".to_string())
        .spawn(|| {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!("event delivery sweeper: runtime build failed: {e}");
                    return;
                }
            };
            rt.block_on(async {
                let db = match Database::open_default() {
                    Ok(db) => db,
                    Err(e) => {
                        tracing::warn!("event delivery sweeper: db open failed: {e}");
                        return;
                    }
                };
                let http = reqwest::Client::new();
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    if let Err(e) = run_event_delivery_sweep(&db, &http).await {
                        tracing::warn!("event delivery sweep error: {e}");
                    }
                }
            });
        })
        .ok();
}

/// Drive the durable webhook delivery executor once. Picks due, not-yet-delivered,
/// not-dead-lettered delivery rows and POSTs each event to its route's signed
/// webhook, advancing the retry schedule.
///
/// Logs a one-line summary only when deliveries were actually attempted
/// (`examined > 0`), keeping quiet sweeps silent. The summary carries counts
/// only — never URLs, bodies, signatures, or secrets.
async fn run_event_delivery_sweep(db: &Database, http: &reqwest::Client) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    let report = envelope_email_transport::event_delivery::deliver_due_events(
        db,
        http,
        now,
        envelope_email_transport::event_delivery::DeliveryLimits::default(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("delivery executor error: {e}"))?;

    if report.examined > 0 {
        info!(
            "event delivery sweep: examined {} delivered {} retried {} dead_lettered {} skipped {}",
            report.examined, report.delivered, report.retried, report.dead_lettered, report.skipped
        );
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

/// Outcome of persisting the local sent state after SMTP acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SentPersistence {
    /// `status='sent'` was durably recorded: safe to clean up the provider
    /// draft copy and report success.
    Recorded,
    /// Persistence failed after a real transmission. `parked` reports whether
    /// the anti-duplicate fallback (moving the draft out of the sweep's due
    /// query) succeeded.
    Unrecorded { parked: bool },
}

/// Terminally park a claimed draft when delivery may have occurred.
///
/// This is intentionally distinct from `release_claim`: `park_delivery_uncertain`
/// atomically clears both the lease and `send_after`, ensuring an inconclusive
/// SMTP attempt cannot remain presented as scheduled or become resendable.
fn park_delivery_uncertain(db: &Database, draft_id: &str, lease: &str, cause: &str) -> bool {
    match db.park_delivery_uncertain(draft_id, lease) {
        Ok(true) => {
            tracing::error!(
                "scheduled send: draft {draft_id} parked as delivery_uncertain after {cause}. \
                 Reconcile explicitly: verify delivery (Sent folder / recipient), then discard \
                 the draft. It will never be re-sent automatically."
            );
            true
        }
        Ok(false) => {
            tracing::error!(
                "scheduled send: draft {draft_id} park after {cause} matched no owned \
                 `sending` row"
            );
            false
        }
        Err(park_err) => {
            tracing::error!(
                "scheduled send: draft {draft_id} could not be parked after {cause}: \
                 {park_err} — it remains claimed as `sending` (never due, never re-sent) \
                 until repaired"
            );
            false
        }
    }
}

/// Persist the sent state for a draft whose SMTP transmission was accepted.
///
/// A `mark_draft_sent` failure is dangerous in both directions: reporting
/// success would lie about durability, and returning the draft to due would
/// resend delivered mail. On failure this parks the claim as the terminal
/// `delivery_uncertain` state — atomically clearing `send_after` under the
/// owner lease — which is non-editable, non-approvable, non-queueable, and
/// never due, so no approval or sweep can ever promote it back into a send.
/// If the park ALSO fails, the row simply remains in its durable `sending`
/// claim — which the due query never selects. Both failures are loud, with
/// the operator reconciliation path (verify delivery, then discard) spelled
/// out.
fn persist_sent_state(
    db: &Database,
    draft_id: &str,
    lease: &str,
    message_id: &str,
) -> SentPersistence {
    match db.mark_draft_sent(draft_id, lease, Some(message_id)) {
        Ok(()) => SentPersistence::Recorded,
        Err(e) => {
            tracing::error!(
                "scheduled send: draft {draft_id} was transmitted but sent-state \
                 persistence failed: {e}"
            );
            let parked =
                park_delivery_uncertain(db, draft_id, lease, "sent-state persistence failure");
            SentPersistence::Unrecorded { parked }
        }
    }
}

/// Best-effort deletion of the original server-side draft copy after a
/// successful, durably recorded scheduled SMTP send.
///
/// Delegates to the shared identity-safe primitives
/// (`envelope_email_transport::draft_cleanup`): the folder comes only from
/// the detected-folder cache, and only the single exact Message-ID match is
/// deleted — zero/ambiguous matches skip. Every skip/failure is logged
/// (draft id, UID, folder only — never addresses or content) and never
/// claimed as done; send success stays authoritative regardless.
async fn cleanup_provider_draft_after_send(state: &AppState, draft: &envelope_email_store::Draft) {
    use envelope_email_transport::draft_cleanup::{
        ProviderDraftCleanup, delete_provider_draft_exact, resolve_draft_cleanup_target,
    };

    let target = {
        let db = state.db.lock().await;
        resolve_draft_cleanup_target(&db, draft)
    };
    let target = match target {
        Ok(target) => target,
        Err(reason) => {
            tracing::warn!(
                "scheduled send: draft {} sent; skipping provider draft cleanup \
                 (provider copy left in place): {reason}",
                draft.id
            );
            return;
        }
    };
    let folder = &target.folder;

    let (client_arc, _creds) = match state.get_or_create_imap(&draft.account_id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "scheduled send: draft {} sent, but IMAP connect for draft cleanup \
                 failed (provider copy left in {folder}): {e}",
                draft.id
            );
            return;
        }
    };
    let mut client = client_arc.lock().await;

    match delete_provider_draft_exact(&mut client, &target).await {
        Ok(ProviderDraftCleanup::Deleted { uid: deleted_uid }) => info!(
            "scheduled send: removed provider draft copy for draft {} \
             (UID {deleted_uid} in {folder})",
            draft.id
        ),
        Ok(ProviderDraftCleanup::Skipped(reason)) => tracing::warn!(
            "scheduled send: draft {} sent; {reason} in {folder} — skipping cleanup",
            draft.id
        ),
        Err(e) => {
            tracing::warn!(
                "scheduled send: draft {} sent, but provider draft cleanup failed \
                 in {folder} (provider copy left in place): {e}",
                draft.id
            );
            drop(client);
            state.evict_imap(&draft.account_id).await;
        }
    }
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
///
/// `human_approved` is read from the draft's durable attestation
/// ([`envelope_email_store::Draft::human_approved`], written only by human
/// surfaces such as the dashboard approve/send actions) and declares the
/// `tyler_approved` attribute to Governor's blind scoring. It is an input
/// attribute, never a bypass: the fail-closed gate still runs in full.
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
        human_approved: draft.human_approved(),
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

// ── Envelope v2 webmail SPA serving ──────────────────────────────────

/// Serve the v2 SPA shell (`web/build/index.html`) — the dashboard entry point.
async fn spa_shell() -> Response {
    match WebAssets::get_file("index.html") {
        Some(bytes) => Html(String::from_utf8_lossy(&bytes).into_owned()).into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "v2 webmail bundle missing from embedded assets (run ci/build-frontend.sh)",
        )
            .into_response(),
    }
}

/// Root fallback: return a real embedded `web/build/` asset by request path
/// (e.g. `/_app/immutable/...`, `/favicon.svg`) with its guessed content type,
/// or the SPA shell for any client-side route (`/cockpit`, `/mail/...`) so the
/// SvelteKit router — built with `paths.base = ''` — resolves it instead of
/// 404ing.
async fn spa_fallback(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if let Some(bytes) = WebAssets::get_file(path) {
        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        return Response::builder()
            .header(header::CONTENT_TYPE, content_type)
            .body(axum::body::Body::from(bytes))
            .unwrap();
    }
    spa_shell().await
}

/// JSON 404 for unmatched `/api/*` paths (keeps API errors machine-readable
/// instead of returning the SPA shell HTML via the root fallback).
async fn api_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "not_found" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {

    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use envelope_email_store::{CredentialBackend, Database};
    use tower::ServiceExt;

    #[test]
    fn broad_bind_requires_a_bearer_token() {
        let broad = "0.0.0.0".parse().unwrap();
        let identity_only = AuthConfig::from_parts(None, ["operator@tailnet.ts.net".to_string()]);
        let token_and_identity = AuthConfig::from_parts(
            Some("dashboard-token".to_string()),
            ["operator@tailnet.ts.net".to_string()],
        );
        let token = AuthConfig::from_parts(Some("dashboard-token".to_string()), []);

        assert!(validate_dashboard_bind(broad, 3141, &AuthConfig::disabled()).is_err());
        assert!(validate_dashboard_bind(broad, 3141, &identity_only).is_err());
        assert!(validate_dashboard_bind(broad, 3141, &token_and_identity).is_err());
        assert!(validate_dashboard_bind(broad, 3141, &token).is_ok());
        assert!(
            validate_dashboard_bind(IpAddr::V4(Ipv4Addr::LOCALHOST), 3141, &identity_only).is_ok()
        );
    }

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
        db.update_draft_imap_uid(&draft.id, 38103).unwrap();
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
    async fn event_delivery_sweep_invokes_executor_on_due_delivery() {
        // Wiring test: a due delivery pointed at an unreachable loopback URL must
        // be picked up by run_event_delivery_sweep and advanced by the executor
        // (connection failure -> attempt recorded + rescheduled), proving the
        // sweep actually drives deliver_due_events. No real webhook is required.
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

        let route = db
            .create_event_route(
                "acc1",
                r#"{"event_types":["agent_action"]}"#,
                // Port 1 is reserved/unbindable: guarantees a fast connect failure.
                r#"{"type":"webhook","url":"http://127.0.0.1:1/hook"}"#,
                true,
                100,
            )
            .unwrap();
        let event = db
            .emit_catalog_event(
                "acc1",
                envelope_email_store::event_catalog::AGENT_ACTION,
                Some(serde_json::json!({"action_type": "move"})),
                Some("agent-1"),
            )
            .unwrap();
        db.enqueue_delivery(
            "del-1",
            &event.id,
            &route.id,
            "dk-1",
            "2000-01-01T00:00:00Z",
        )
        .unwrap();

        // Before the sweep the delivery is pending with zero attempts.
        let before = db.get_delivery("del-1").unwrap().unwrap();
        assert_eq!(before.attempt_count, 0);

        let http = reqwest::Client::new();
        run_event_delivery_sweep(&db, &http).await.unwrap();

        // After the sweep the executor attempted (and rescheduled) the delivery.
        let after = db.get_delivery("del-1").unwrap().unwrap();
        assert_eq!(
            after.attempt_count, 1,
            "the sweep must drive the delivery executor over the due row"
        );
        assert!(
            after.delivered_at.is_none(),
            "an unreachable webhook must not be marked delivered"
        );
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

    /// Scheduled attribution must declare `tyler_approved` only from the
    /// durable human attestation — never from agent-created state alone — so a
    /// human-approved send does not come back from Governor as review_required
    /// while agents can never self-approve. Pure: no Governor, no network.
    #[test]
    fn scheduled_send_context_declares_tyler_approved_only_with_human_attestation() {
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
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        // Agent-written contextual metadata is not an approval.
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({"in_reply_to": "parent@example.net"}),
        )
        .unwrap();

        let unapproved = db.get_draft(&draft.id).unwrap().unwrap();
        let attrs = scheduled_send_context(&unapproved, Some("example.com".to_string()), &[])
            .to_governor_attrs();
        assert!(
            !attrs.contains(&"tyler_approved"),
            "agent state alone must not declare tyler_approved: {attrs:?}"
        );

        // The dashboard approve/send action records the durable attestation
        // (revision-bound CAS); the next sweep's re-derivation declares
        // tyler_approved.
        let human_view = db.get_draft(&draft.id).unwrap().unwrap();
        db.record_draft_human_approval(
            &draft.id,
            human_view.revision,
            "human:dashboard",
            "2026-07-10T09:00:00Z",
        )
        .unwrap();
        let approved = db.get_draft(&draft.id).unwrap().unwrap();
        let attrs = scheduled_send_context(&approved, Some("example.com".to_string()), &[])
            .to_governor_attrs();
        assert!(attrs.contains(&"tyler_approved"), "{attrs:?}");
        // Threading survived the attestation merge and still attributes.
        assert!(attrs.contains(&"reply_to_thread"), "{attrs:?}");

        // Revision binding at the attribution boundary: a content edit after
        // approval (e.g. an agent modifying the queued draft) clears the
        // attestation, so the sweep re-scores WITHOUT tyler_approved.
        db.update_draft_content(
            &draft.id,
            Some("other@example.net"),
            None,
            None,
            None,
            Some("changed after approval"),
            None,
        )
        .unwrap();
        let edited = db.get_draft(&draft.id).unwrap().unwrap();
        let attrs = scheduled_send_context(&edited, Some("example.com".to_string()), &[])
            .to_governor_attrs();
        assert!(
            !attrs.contains(&"tyler_approved"),
            "an edited revision must not ride the earlier approval: {attrs:?}"
        );
    }

    /// End-to-end regression for the stale-alternative bug: a dashboard
    /// text-body edit must be what actually goes on the wire.
    ///
    /// The dashboard editor POSTs `text_content` alone for a draft that carries
    /// both a text and an HTML body. When the omitted HTML survived the edit,
    /// the due-send snapshot stayed dual-body and the sweep handed both forms to
    /// `build_message`, producing `multipart/alternative` — receiving clients
    /// prefer the HTML alternative, so the recipient read the UNEDITED draft.
    ///
    /// This runs the real edit handler, takes the row the sweep's due scan
    /// returns, and builds the message from the same body arguments the sweep
    /// hands to `SmtpSender::send`. No socket is opened — `build_message` only
    /// constructs MIME.
    #[tokio::test]
    async fn dashboard_text_edit_is_what_the_due_send_snapshot_transmits() {
        use axum::extract::{Path as AxumPath, State as AxumState};
        use envelope_email_store::models::{Account, AccountWithCredentials};

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
                Some("Quote request"),
                Some("OLD text body"),
                Some("<p>OLD html body</p>"),
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        let viewed = db.get_draft(&draft.id).unwrap().unwrap();
        let state = AppState::new(db, CredentialBackend::File);

        // The dashboard editor's POST: edited text, no HTML field.
        let response = handlers::drafts::edit(
            AxumState(state.clone()),
            AxumPath(("acc1".to_string(), draft.id.clone())),
            axum::Json(handlers::drafts::DraftEditRequest {
                expected_revision: viewed.revision,
                to_addr: None,
                cc_addr: None,
                bcc_addr: None,
                subject: None,
                text_content: Some("NEW text body".to_string()),
                html_content: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        // Queue it, then take the row the sweep's due scan returns.
        let queued = {
            let db = state.db.lock().await;
            db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
                .unwrap();
            db.list_drafts_due_for_send()
                .unwrap()
                .into_iter()
                .find(|d| d.id == draft.id)
                .expect("edited draft should be due for send")
        };

        let account = Account {
            id: "acc1".to_string(),
            name: "Agent".to_string(),
            username: "agent@example.com".to_string(),
            domain: "example.com".to_string(),
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            smtp_username: None,
            imap_username: None,
            display_name: None,
            signature_text: None,
            signature_html: None,
            created_at: String::new(),
        };
        let creds = AccountWithCredentials {
            account,
            password: "unused".to_string(),
            smtp_password: None,
            imap_password: None,
        };
        // Same body arguments the sweep passes to `SmtpSender::send`.
        let (message, _) = envelope_email_transport::smtp::build_message(
            &creds,
            "regression@example.com",
            &queued.to_addr,
            queued.subject.as_deref().unwrap_or(""),
            queued.text_content.as_deref(),
            queued.html_content.as_deref(),
            None,
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .unwrap();
        let wire = String::from_utf8_lossy(&message.formatted()).to_string();

        assert!(
            wire.contains("NEW text body"),
            "the edited body must be on the wire"
        );
        assert!(
            !wire.contains("OLD html body"),
            "the pre-edit HTML alternative must not be transmitted — clients \
             prefer it over the edited text and would render the unedited draft"
        );
        assert!(
            !wire.contains("multipart/alternative"),
            "a single-body draft must not be sent as multipart/alternative"
        );
    }

    /// After SMTP acceptance, a sent-state persistence failure must never look
    /// like durable success and must not leave the draft re-sendable by the
    /// next sweep (duplicate transmission). No SMTP or mailbox involved — this
    /// exercises only the local persistence decision.
    #[test]
    fn unrecorded_sent_state_parks_draft_out_of_sweep_instead_of_resending() {
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
                "to@example.net",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let revision = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let lease = db
            .claim_draft_for_sending(&draft.id, revision)
            .unwrap()
            .expect("precondition: sweep claims the due draft before SMTP");

        // Simulate a persistence failure for exactly the sent-state write:
        // `mark_draft_sent` is the only path that touches `sent_at`.
        db.conn()
            .execute(
                "CREATE TRIGGER fail_sent_write BEFORE UPDATE OF sent_at ON drafts
                 BEGIN SELECT RAISE(ABORT, 'simulated disk failure'); END",
                [],
            )
            .unwrap();

        let outcome = persist_sent_state(&db, &draft.id, &lease, "<mid@example.com>");
        assert_eq!(outcome, SentPersistence::Unrecorded { parked: true });

        let parked = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(parked.status, DraftStatus::DeliveryUncertain);
        assert!(
            parked.sent_at.is_none(),
            "sent state really did not persist"
        );
        assert!(
            parked.send_after.is_none(),
            "the park must clear send_after atomically"
        );
        assert!(
            !db.list_drafts_due_for_send()
                .unwrap()
                .iter()
                .any(|d| d.id == draft.id),
            "an unrecorded send must not be re-selected and resent by the sweep"
        );
        // Approval/queue must reject the terminal-recovery state outright.
        assert!(
            db.approve_draft_revision(
                &draft.id,
                parked.revision,
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .is_err()
        );
        assert!(
            db.queue_draft_with_human_approval(
                &draft.id,
                parked.revision,
                "2026-07-10T09:02:00Z",
                "human:dashboard",
                "2026-07-10T09:00:00Z",
            )
            .is_err()
        );

        // Explicit operator reconciliation is the only exit: discard works.
        assert!(db.discard_draft(&draft.id).unwrap());

        // Happy path: with persistence working, the state is durably `sent`.
        db.conn()
            .execute("DROP TRIGGER fail_sent_write", [])
            .unwrap();
        let fresh = db
            .create_draft(
                "acc1",
                "to@example.net",
                Some("Queued again"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&fresh.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let lease2 = db
            .claim_draft_for_sending(&fresh.id, fresh.revision)
            .unwrap()
            .expect("re-claim");
        assert_eq!(
            persist_sent_state(&db, &fresh.id, &lease2, "<mid@example.com>"),
            SentPersistence::Recorded
        );
        let sent = db.get_draft(&fresh.id).unwrap().unwrap();
        assert_eq!(sent.status, DraftStatus::Sent);
        assert!(sent.sent_at.is_some());
    }

    #[test]
    fn inconclusive_smtp_result_clears_the_scheduled_delivery_state() {
        use envelope_email_store::DraftStatus;

        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, \
                 imap_host, imap_port, encrypted_password) \
                 VALUES ('acc1', 'Agent', 'agent@example.com', 'example.com', \
                         'smtp.example.com', 587, 'imap.example.com', 993, 'encrypted')",
                [],
            )
            .unwrap();
        let draft = db
            .create_draft(
                "acc1",
                "to@example.net",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let lease = db
            .claim_draft_for_sending(&draft.id, draft.revision)
            .unwrap()
            .expect("due draft is claimed before SMTP");

        assert!(park_delivery_uncertain(
            &db,
            &draft.id,
            &lease,
            "a simulated SMTP error"
        ));

        let parked = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(parked.status, DraftStatus::DeliveryUncertain);
        assert!(parked.send_after.is_none(), "terminal park clears schedule");
        let operation_token: Option<String> = db
            .conn()
            .query_row(
                "SELECT operation_token FROM drafts WHERE id = ?1",
                [&draft.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(operation_token.is_none(), "terminal park clears lease");
        assert!(
            !db.list_drafts_due_for_send()
                .unwrap()
                .iter()
                .any(|candidate| candidate.id == draft.id),
            "an inconclusive SMTP attempt must not remain visible as due"
        );
    }

    /// When even the anti-duplicate park fails, the outcome must say so —
    /// callers never treat it as recorded, and both failures are loud.
    #[test]
    fn unrecorded_sent_state_reports_failed_park() {
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
                "to@example.net",
                Some("Queued"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        // The sweep claims before transmitting; both post-send updates then
        // fail: a status-write failure breaks mark_draft_sent (which sets
        // status='sent') AND the blocked-park fallback.
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();
        let revision = db.get_draft(&draft.id).unwrap().unwrap().revision;
        let lease = db
            .claim_draft_for_sending(&draft.id, revision)
            .unwrap()
            .expect("claim");
        db.conn()
            .execute(
                "CREATE TRIGGER fail_status_write BEFORE UPDATE OF status ON drafts
                 BEGIN SELECT RAISE(ABORT, 'simulated disk failure'); END",
                [],
            )
            .unwrap();

        assert_eq!(
            persist_sent_state(&db, &draft.id, &lease, "<mid@example.com>"),
            SentPersistence::Unrecorded { parked: false }
        );

        // Even with BOTH post-send updates failing, the durable `sending`
        // claim keeps the transmitted draft out of the due query — the failed
        // park can never become a duplicate retransmission.
        let stranded = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(stranded.status, envelope_email_store::DraftStatus::Sending);
        assert!(
            !db.list_drafts_due_for_send()
                .unwrap()
                .iter()
                .any(|d| d.id == draft.id),
            "a transmitted draft must never be re-selected, even when both \
             mark-sent and the park fallback fail"
        );
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
    async fn drafts_by_imap_uid_endpoint_resolves_to_reviewable_local_draft() {
        let (state, draft_id, _) = test_state();
        let app = dashboard_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/accounts/editor@spainexpat.com/drafts/by-imap-uid/38103")
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
        assert_eq!(json["draft"]["imap_uid"], 38103);
        assert_eq!(json["source"]["kind"], "imap_uid");
        assert_eq!(
            json["dashboard_path"],
            format!("/accounts/acc1/drafts/{draft_id}")
        );
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
        assert!(html.contains("/_app/"));
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
                html.contains("<title>Envelope</title>") && html.contains("/_app/"),
                "deep link {uri} should return the dashboard SPA index"
            );
        }
    }

    /// Client route id of the draft review composer, as SvelteKit compiles it
    /// into the embedded bundle's route table.
    const DRAFT_REVIEW_ROUTE_ID: &str = "/accounts/[account]/drafts/[draft]";

    /// A route id that has shipped since the v2 cutover. Used as a control so a
    /// SvelteKit/Vite change to the route-table encoding fails this test loudly
    /// instead of silently making the draft assertion vacuous.
    const CONTROL_ROUTE_ID: &str = "/mail/[box]/[account]/[uid]";

    /// Source of the SvelteKit client entry chunk inside the embedded
    /// `web/build/` bundle. It carries the client route table, e.g.
    /// `{"/":[3],"/cockpit":[4],"/mail/[box]":[6,[2]], …}`.
    fn embedded_spa_entry_chunk() -> String {
        let path = WebAssets::iter()
            .map(|file| file.to_string())
            .find(|path| path.starts_with("_app/immutable/entry/app.") && path.ends_with(".js"))
            .expect("embedded SPA bundle must contain a SvelteKit client entry chunk");
        String::from_utf8(WebAssets::get_file(&path).expect("entry chunk readable"))
            .expect("entry chunk is utf-8")
    }

    /// True when a concrete path is matched by a SvelteKit route id: same
    /// segment count, literal segments equal, `[param]` segments absorb any
    /// non-empty segment.
    fn route_id_matches(route_id: &str, path: &str) -> bool {
        let pattern: Vec<&str> = route_id.split('/').collect();
        let actual: Vec<&str> = path.split('/').collect();
        pattern.len() == actual.len()
            && pattern.iter().zip(&actual).all(|(expected, segment)| {
                if expected.starts_with('[') && expected.ends_with(']') {
                    !segment.is_empty()
                } else {
                    expected == segment
                }
            })
    }

    /// Regression for the generated draft review link 404: the axum SPA
    /// fallback already served the shell for `/accounts/<id>/drafts/<id>`
    /// (see the deep-link tests above), but the SvelteKit bundle had no
    /// matching client route, so the router rendered its own 404 page. Serving
    /// the shell is necessary and not sufficient — this asserts the embedded
    /// bundle can actually route the path the CLI and API hand to humans.
    #[test]
    fn embedded_spa_bundle_routes_the_generated_draft_review_link() {
        let entry = embedded_spa_entry_chunk();

        assert!(
            entry.contains(&format!("\"{CONTROL_ROUTE_ID}\"")),
            "control route {CONTROL_ROUTE_ID} missing from the embedded route table — the \
             SvelteKit route-table encoding changed and this assertion needs updating"
        );
        assert!(
            entry.contains(&format!("\"{DRAFT_REVIEW_ROUTE_ID}\"")),
            "embedded SPA bundle has no {DRAFT_REVIEW_ROUTE_ID} client route, so generated \
             draft links render the SvelteKit 404 page — rebuild with ci/build-frontend.sh"
        );

        // The exact link shape the CLI (`draft_dashboard_url`) and the drafts
        // API (`dashboard_url` / `review_url`) emit must match that route.
        let generated = crate::ui_paths::draft_dashboard_path(
            "31f5fddf-04f9-4978-aea5-29aa9af12bb0",
            "365d958c-6666-4872-898e-cb8a60f21aca",
        );
        assert_eq!(
            generated,
            "/accounts/31f5fddf-04f9-4978-aea5-29aa9af12bb0/drafts/365d958c-6666-4872-898e-cb8a60f21aca"
        );
        assert!(
            route_id_matches(DRAFT_REVIEW_ROUTE_ID, &generated),
            "generated draft link {generated} is not matched by client route {DRAFT_REVIEW_ROUTE_ID}"
        );
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
