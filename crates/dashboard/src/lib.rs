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
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use envelope_email_store::{CredentialBackend, Database};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{info, warn};

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

    {
        let state = state.clone();
        tokio::spawn(async move { backfill_address_history(&state).await });
    }

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

/// Local backfill of the compose autocomplete address history, once per
/// account, resumable across restarts.
///
/// Address history is reconciled whenever `envelope thread` scans a mailbox or
/// something POSTs `/api/messages/unified/refresh`. Without this pass, an
/// install whose thread cache and message index were already populated before
/// this feature existed would offer no suggestions until the next refresh — a
/// working feature that looks broken.
///
/// Everything here is local SQL over rows already on disk: no IMAP and no
/// credential decryption. It runs as a background task rather than before the
/// listener because the first pass over an established install walks the whole
/// thread cache; the store hands it back in bounded chunks and records how far
/// it got, so the dashboard answers requests throughout and a restart mid-pass
/// resumes instead of starting over. A failure only warns — recipient
/// autocomplete is never worth refusing to start the dashboard over.
async fn backfill_address_history(state: &AppState) {
    let accounts = {
        let db = state.db.lock().await;
        match db.list_accounts() {
            Ok(accounts) => accounts,
            Err(e) => {
                tracing::warn!("address history backfill skipped; could not list accounts: {e}");
                return;
            }
        }
    };
    for account in accounts {
        handlers::address_book::catch_up_account(state, &account.id).await;
    }
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
        // Recipient autocomplete for the compose surfaces (read-only, local
        // address history — never IMAP).
        .route(
            "/accounts/{id}/address-suggestions",
            get(handlers::address_book::suggest),
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
        .route(
            "/accounts/{id}/messages/{uid}/snooze",
            post(handlers::messages::snooze),
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
            "/accounts/{id}/drafts/{draft_id}/hold",
            post(handlers::drafts::hold),
        )
        .route(
            "/accounts/{id}/drafts/{draft_id}/block",
            post(handlers::drafts::block),
        )
        .route(
            "/accounts/{id}/drafts/{draft_id}/send",
            post(handlers::drafts::send),
        )
        // Draft attachments. The bytes live in the draft row, so download
        // reads the stored snapshot rather than IMAP — an unsent draft's
        // files exist nowhere else. Upload/remove are revision-guarded edits.
        .route(
            "/accounts/{id}/drafts/{draft_id}/attachments",
            post(handlers::draft_attachments::upload),
        )
        .route(
            "/accounts/{id}/drafts/{draft_id}/attachments/{filename}",
            get(handlers::draft_attachments::download).delete(handlers::draft_attachments::remove),
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
    //
    // The three `/accounts/...` routes below are the pre-1.0.11 link shapes,
    // which the SPA has no client route for — the fallback served the shell and
    // the SvelteKit router then rendered its own 404 inside a 200. They are
    // matched here, ahead of the fallback, and redirected to the canonical
    // routes so links already in agent transcripts, notifications, and mail keep
    // resolving. `/api/accounts/...` is a separate nest and is unaffected.
    Router::new()
        .route("/", get(spa_shell))
        .route(
            "/accounts/{account}/messages/{uid}",
            get(legacy_message_redirect),
        )
        .route("/accounts/{account}/cockpit", get(legacy_cockpit_redirect))
        .route("/accounts/{account}/rules", get(legacy_rules_redirect))
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

pub(crate) async fn run_scheduled_send_sweep(state: &AppState) -> anyhow::Result<()> {
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

        // ── Attribution inputs from the draft's DURABLE state ──
        //
        // Load the declaration the bot validated at queue time and decide whether
        // a declaration is required. A human-attested draft (revision-bound) does
        // not require a bot declaration and lets Envelope derive `tyler_approved`;
        // every other draft is bot-originated and a non-empty derived set never
        // excuses a missing/stale declaration. A material edit bumps the revision,
        // which makes the persisted declaration stale (dropped) and resets the
        // attempt counter for free.
        let persisted =
            envelope_email_transport::attribution_persist::PersistedDeclaration::from_metadata(
                draft.metadata.as_ref(),
            );
        let (declared, require_declaration) =
            envelope_email_transport::attribution_persist::scheduled_attribution_inputs(
                draft.created_by.as_deref(),
                draft.human_approved(),
                persisted.as_ref(),
                draft.revision,
            );
        let prior_attempts = persisted
            .as_ref()
            .filter(|d| d.is_current(draft.revision))
            .map(|d| d.attempts)
            .unwrap_or(0);

        // ── Governor gate (fail-closed before any real SMTP) ──
        //
        // The scheduled-send sweep is the one place that actually transmits
        // queued mail, so it must run the Governor gate against the reloaded,
        // claimed row. When Governor is required and missing/errors/denies/
        // reviews, the send is refused and the claim is released per reason.
        let gov_outcome = run_governor_gate(
            state,
            draft,
            &creds,
            subject,
            &attachments,
            &declared,
            require_declaration,
        )
        .await;
        if !gov_outcome.allowed {
            // ── Bot-originated attribution failure: bounded correction loop ──
            //
            // The declaration is missing/stale/invalid for this bot draft (never
            // declared, or a material edit invalidated it). Governor was NOT
            // spawned. Retry a bounded number of times so an async correction can
            // land, then park for human review — never a retry storm.
            if gov_outcome.is_attribution_failure() {
                use envelope_email_transport::attribution_persist::{
                    AttributionFailureAction, attribution_failure_action, scheduled_origin,
                };
                // The correction loop is decided from the draft's DURABLE origin.
                // Bot/Unknown origins run the bounded retry→park loop with their
                // bot provenance preserved; a genuinely human-originated draft
                // (stale approval) is parked for human re-approval WITHOUT
                // fabricating any bot declaration — its human origin survives so a
                // fresh attestation can recover it.
                let origin = scheduled_origin(
                    draft.created_by.as_deref(),
                    persisted.as_ref(),
                    draft.revision,
                );
                let action =
                    attribution_failure_action(origin, &declared, draft.revision, prior_attempts);
                let outcome_label = match &action {
                    AttributionFailureAction::HumanReview
                    | AttributionFailureAction::Park { .. } => "blocked",
                    AttributionFailureAction::Retry { .. } => "deferred",
                };
                let persisted_ok = {
                    let db = state.db.lock().await;
                    match &action {
                        AttributionFailureAction::HumanReview => {
                            // Park for re-approval with a user-facing reason.
                            // Silent pending_review is not allowed.
                            db.park_for_review_with_block(
                                &draft.id,
                                &lease,
                                &serde_json::json!({
                                    "code": "attributes_required",
                                    "title": "This send was stopped",
                                    "explanation": "Envelope paused this message before sending because it was missing a required fact label. Nothing was transmitted.",
                                    "action": "send"
                                }),
                            )
                            .unwrap_or(false)
                        }
                        AttributionFailureAction::Park { value } => db
                            .park_attribution_exhausted(&draft.id, &lease, value)
                            .unwrap_or(false),
                        AttributionFailureAction::Retry { value } => db
                            .defer_attribution_retry(&draft.id, &lease, value)
                            .unwrap_or(false),
                    }
                };
                tracing::warn!(
                    "scheduled send: draft {} failed attribution at SMTP time ({}); origin={} — {}{}",
                    draft.id,
                    gov_outcome
                        .block_code
                        .clone()
                        .unwrap_or_else(|| "attributes_required".to_string()),
                    origin.as_str(),
                    match &action {
                        AttributionFailureAction::HumanReview =>
                            "parking pending_review for human re-approval (human origin preserved)"
                                .to_string(),
                        AttributionFailureAction::Park { .. } =>
                            "parking pending_review (attribution_exhausted)".to_string(),
                        AttributionFailureAction::Retry { value } => format!(
                            "left due for correction (attempt {} of {})",
                            value["attempts"],
                            envelope_email_transport::attribution_persist::MAX_ATTRIBUTION_ATTEMPTS
                        ),
                    },
                    if persisted_ok {
                        ""
                    } else {
                        " [WARNING: transition matched no owned row; the claim stays inert as `sending`]"
                    },
                );
                // Only claim the blocked/deferred transition when it actually
                // persisted. On an owner-token mismatch / concurrent transition
                // the helper returned false and the claim stays inert as
                // `sending`; publishing `blocked`/`deferred` there would be a lie,
                // so emit a truthful `transition_failed` diagnostic instead.
                let published_outcome = transition_outcome(persisted_ok, outcome_label);
                state
                    .events
                    .publish(crate::events::DashboardEvent::SendStatus {
                        account_id: draft.account_id.clone(),
                        draft_id: draft.id.clone(),
                        outcome: published_outcome,
                        governor_decision: Some(gov_outcome.decision.clone()),
                        governor_block_code: gov_outcome.block_code.clone(),
                    });
                continue;
            }

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
            // A durable review verdict parks pending_review AND clears send_after
            // (`park_review_claim`) so no surface can present it as queued/due; a
            // transient gate failure releases back to `draft` WITH send_after
            // intact so a later sweep retries once Governor is reachable.
            let released = if pause_for_review {
                park_review_claim_with_block(
                    state,
                    &draft.id,
                    &lease,
                    &serde_json::json!({
                        "code": gov_outcome.block_code.clone().unwrap_or_else(|| "governor_blocked".to_string()),
                        "title": "This send was stopped",
                        "explanation": "Envelope paused this message for review before sending. Nothing was transmitted.",
                        "action": "send"
                    }),
                )
                .await
            } else {
                release_claim(
                    state,
                    &draft.id,
                    &lease,
                    envelope_email_store::DraftStatus::Draft,
                )
                .await
            };
            // Metadata-level send status: decision + optional block code only.
            // No recipients, subject, or body ever cross this channel. Publish
            // the blocked/deferred outcome ONLY when the transition actually
            // persisted; on an owner-token mismatch/failed release the claim is
            // inert as `sending`, so emit a truthful `transition_failed` instead
            // of a false blocked/deferred.
            let outcome = transition_outcome(
                released,
                if pause_for_review {
                    "blocked"
                } else {
                    "deferred"
                },
            );
            state
                .events
                .publish(crate::events::DashboardEvent::SendStatus {
                    account_id: draft.account_id.clone(),
                    draft_id: draft.id.clone(),
                    outcome,
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
            draft_from_override(draft),
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
                    // Sent-folder proof parity with the immediate CLI/MCP paths:
                    // resolve (and client-append when needed) the Sent copy and
                    // persist truthful proof/UID. Strictly after durable sent-state
                    // persistence, and best-effort — a Sent-copy failure never
                    // downgrades the confirmed send.
                    resolve_and_record_sent_copy(
                        creds,
                        draft,
                        &message_id,
                        subject,
                        &attachments,
                        thread_in_reply_to.as_deref(),
                        &thread_references,
                    )
                    .await;
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

/// Return the explicit sending identity persisted by draft create/edit.
/// Scheduled sends must use the same identity as immediate sends and the
/// provider Drafts copy; falling back remains the SMTP account default.
fn draft_from_override(draft: &envelope_email_store::Draft) -> Option<&str> {
    draft
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("from"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|from| !from.is_empty())
}

/// The truthful send-status outcome for a claim transition: the `intended`
/// label (`blocked`/`deferred`) ONLY when the owned transition actually
/// persisted, otherwise the inert `transition_failed` diagnostic. An owner-token
/// mismatch / concurrent transition leaves the claim inert as `sending`, so the
/// sweep must never publish a `blocked`/`deferred`/`parked` outcome that did not
/// happen.
fn transition_outcome(persisted: bool, intended: &'static str) -> &'static str {
    if persisted {
        intended
    } else {
        "transition_failed"
    }
}

/// Release a sweep claim into `to`, logging (but never panicking on) failure.
/// Only ever called before SMTP acceptance. A transmitted or inconclusive
/// delivery leaves the claim through `park_delivery_uncertain` or
/// `mark_draft_sent`. If the release fails, the row stays `sending`: stranded
/// but inert, and never re-selected for a duplicate transmission.
///
/// Returns `true` only when the owned `sending` row was actually transitioned.
/// A `false` (owner-token mismatch, already transitioned, or DB error) means the
/// intended `to` state did NOT persist — callers must NOT publish an outcome
/// (blocked/deferred) that claims it did.
async fn release_claim(
    state: &AppState,
    draft_id: &str,
    lease: &str,
    to: envelope_email_store::DraftStatus,
) -> bool {
    let db = state.db.lock().await;
    match db.release_sending_draft(draft_id, lease, to.clone()) {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!(
                "scheduled send: claim release for draft {draft_id} matched no `sending` row \
                 (already transitioned?)"
            );
            false
        }
        Err(e) => {
            tracing::error!(
                "scheduled send: claim release for draft {draft_id} → {} failed: {e} — the \
                 draft stays parked in `sending` (inert, never re-sent) until repaired",
                to.as_str()
            );
            false
        }
    }
}

/// Park a `sending` claim as `pending_review` after a durable Governor **review**
/// verdict, clearing `send_after` under the owner lease so no surface (dashboard,
/// CLI, or the due query) can present the parked draft as still queued or show a
/// stale countdown. Persist a user-facing `send_block` so the page cannot stay
/// silent. Nothing is transmitted and no Sent copy is written. Returns
/// `true` only when the owned `sending` row was actually parked.
async fn park_review_claim_with_block(
    state: &AppState,
    draft_id: &str,
    lease: &str,
    block: &serde_json::Value,
) -> bool {
    let db = state.db.lock().await;
    match db.park_for_review_with_block(draft_id, lease, block) {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!(
                "scheduled send: review park for draft {draft_id} matched no owned `sending` \
                 row (already transitioned?)"
            );
            false
        }
        Err(e) => {
            tracing::error!(
                "scheduled send: review park for draft {draft_id} failed: {e} — the draft \
                 stays parked in `sending` (inert, never re-sent) until repaired"
            );
            false
        }
    }
}

/// Resolve and durably record the Sent-folder copy for a scheduled send that was
/// transmitted and recorded `sent`, using the SAME source-aware resolver the
/// immediate CLI/MCP paths use ([`envelope_email_transport::sent_proof`]). The
/// archive copy (when the provider does not auto-file) is rebuilt with the SAME
/// builder and Message-ID that was transmitted, so attachments and threading
/// headers are preserved and a client-appended copy is never labeled provider
/// proof. Best-effort: a failed lookup/append is logged (draft id, folder, uid,
/// copy_source only — never addresses or content) and never downgrades the send.
///
/// The resolver borrows `&Database` across its IMAP awaits, and `Database` is
/// `!Sync`, so it cannot run inside the `Send`-bound scheduled-send sweep future
/// directly. It runs on a dedicated blocking thread with its own current-thread
/// runtime and its own DB connection — the same bridge the event-delivery sweeper
/// uses. `creds` is moved in (it is not needed again in this iteration).
#[allow(clippy::too_many_arguments)]
async fn resolve_and_record_sent_copy(
    creds: envelope_email_store::models::AccountWithCredentials,
    draft: &envelope_email_store::Draft,
    message_id: &str,
    subject: &str,
    attachments: &[envelope_email_transport::smtp::Attachment],
    in_reply_to: Option<&str>,
    references: &[String],
) {
    let draft_id = draft.id.clone();
    let account_id = draft.account_id.clone();
    let to = draft.to_addr.clone();
    let subject = subject.to_string();
    let text = draft.text_content.clone();
    let html = draft.html_content.clone();
    let cc = draft.cc_addr.clone();
    let bcc = draft.bcc_addr.clone();
    let from_override = draft_from_override(draft).map(str::to_string);
    let reply_to = draft.reply_to.clone();
    let in_reply_to = in_reply_to.map(str::to_string);
    let references = references.to_vec();
    let message_id = message_id.to_string();
    let attachments = attachments.to_vec();

    let join = tokio::task::spawn_blocking(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::warn!(
                    "scheduled send: sent-copy runtime build failed for draft {draft_id}: {e}"
                );
                return;
            }
        };
        rt.block_on(async move {
            let db = match Database::open_default() {
                Ok(db) => db,
                Err(e) => {
                    tracing::warn!(
                        "scheduled send: sent-copy db open failed for draft {draft_id}: {e}"
                    );
                    return;
                }
            };
            let provider_type = db.get_provider_type(&account_id).ok().flatten();
            let result = envelope_email_transport::sent_proof::resolve_sent_copy_after_send(
                &db,
                &creds,
                provider_type.as_deref(),
                from_override.as_deref().unwrap_or(""),
                &to,
                &subject,
                text.as_deref(),
                html.as_deref(),
                cc.as_deref(),
                bcc.as_deref(),
                reply_to.as_deref(),
                in_reply_to.as_deref(),
                &references,
                &message_id,
                &attachments,
            )
            .await;
            match db.record_sent_copy_proof(
                &draft_id,
                result.proof.folder.as_deref(),
                result.proof.uid,
                result.proof.lookup_status,
                result.proof.copy_source,
            ) {
                Ok(true) => info!(
                    "scheduled send: recorded Sent copy for draft {draft_id} \
                     (copy_source={}, uid={:?}, folder={:?})",
                    result.proof.copy_source, result.proof.uid, result.proof.folder
                ),
                Ok(false) => tracing::warn!(
                    "scheduled send: Sent-copy proof for draft {draft_id} not recorded \
                     (row is not `sent`)"
                ),
                Err(e) => tracing::warn!(
                    "scheduled send: failed to record Sent-copy proof for draft {draft_id}: {e}"
                ),
            }
        });
    })
    .await;
    if let Err(e) = join {
        tracing::warn!("scheduled send: sent-copy task join failed: {e}");
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
        // Derive `short_body` from the FINAL persisted bodies being transmitted
        // via the one canonical policy, so the sweep corroborates a bot's
        // `short_body` declaration identically to the direct CLI/MCP boundary and
        // for every body shape (text, HTML-only, dual, empty). Always observable.
        short_body: Some(envelope_email_transport::attribution::final_body_is_short(
            draft.text_content.as_deref(),
            draft.html_content.as_deref(),
        )),
        ..Default::default()
    }
}

/// The additive success `attribution` block for a **human-queued** dashboard
/// draft (compose, reply, and the draft queue-for-send). The durable human
/// attestation is the origin: `tyler_approved` is derived from the persisted,
/// revision-bound approval, and **no bot declaration is fabricated**
/// (`declared_attrs` stays empty for a human-originated send). The real Governor
/// decision runs later at the scheduled-send sweep, so the block is deferred
/// (`governor: null`, `governor_decision_pending` set) — matching the CLI/MCP
/// queued success block. Sanitized: never a score, weight, threshold, body, raw
/// recipient, secret, or attachment byte.
///
/// Reflects the same origin/attestation logic the sweep enforces via
/// [`scheduled_attribution_inputs`], so the advertised block matches what the
/// sweep will actually resolve. `account_username` is the account's email — the
/// domain is derived from it exactly as the sweep does ([`account_domain_from_username`]),
/// and the attachment host facts (`has_attachment`, `sensitive_attachment`) are
/// derived from the draft's own persisted snapshots, so an attachment-bearing
/// queue's advertised block agrees with the sweep's derivation rather than
/// omitting those facts.
pub(crate) fn human_queue_attribution_block(
    draft: &envelope_email_store::Draft,
    account_username: &str,
) -> serde_json::Value {
    use envelope_email_transport::attribution::resolve;
    use envelope_email_transport::attribution_persist::{
        PersistedDeclaration, scheduled_attribution_inputs, success_attribution_block,
    };
    let persisted = PersistedDeclaration::from_metadata(draft.metadata.as_ref());
    let (declared, require) = scheduled_attribution_inputs(
        draft.created_by.as_deref(),
        draft.human_approved(),
        persisted.as_ref(),
        draft.revision,
    );
    // A send the operator queued through Human-only Send is transmitted without a
    // bot declaration ([`run_governor_gate`]). The advertised block resolves under
    // the same rule so it can never announce `attributes_required` for a message
    // that will actually go out. Any bot declaration on the row is still reported
    // as-is — the requirement is lifted, nothing is fabricated.
    let require = require && !dashboard_human_send_authorized(draft);
    let account_domain = account_domain_from_username(account_username);
    let attachments = draft_attachment_stubs(draft);
    let ctx = scheduled_send_context(draft, account_domain, &attachments);
    let resolution = resolve(&declared, &ctx, require);
    success_attribution_block(&resolution, None, None, true)
}

/// The account's lowercased mail domain from its username/email — the SAME
/// derivation the scheduled-send sweep uses in [`run_governor_gate`], so a
/// human-queued draft's advertised attribution resolves host facts identically
/// to what the sweep will derive.
pub(crate) fn account_domain_from_username(username: &str) -> Option<String> {
    username
        .rsplit_once('@')
        .map(|(_, d)| d.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty())
}

/// Body-free attachment stubs (filename + content_type only) from a draft's
/// persisted snapshots, for deriving the `has_attachment`/`sensitive_attachment`
/// host facts without rehydrating the snapshotted bytes. Uses the same snapshot
/// fields the sweep decodes, so the derived attribute set is identical.
fn draft_attachment_stubs(
    draft: &envelope_email_store::Draft,
) -> Vec<envelope_email_transport::smtp::Attachment> {
    draft
        .attachments
        .iter()
        .map(|s| envelope_email_transport::smtp::Attachment {
            filename: s
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("attachment")
                .to_string(),
            content_type: s
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream")
                .to_string(),
            data: Vec::new(),
        })
        .collect()
}

/// The surface label the dashboard's **Human-only Send** routes stamp on the
/// send authorization they mint (`handlers::drafts::send`, `handlers::compose`).
pub(crate) const DASHBOARD_SEND_SURFACE: &str = "human:dashboard";

/// True when this draft's pending send is one Tyler queued himself by clicking
/// **Human-only Send** on the dashboard.
///
/// Two facts have to hold at once, and neither is sufficient alone:
///
/// 1. The dashboard's own send route minted the authorization
///    (`queued_by = human:dashboard`). `Database::queue_draft_with_human_send`
///    is its only writer and writes it *as* the queue transition, so it always
///    describes a specific send a human started — not a review decision, and not
///    a send some other path queued afterwards. `Database::queue_draft_for_send`
///    (CLI `draft send`, MCP `send_draft`) strips it, so an agent re-queue takes
///    the pending send back into governed territory.
/// 2. It is still current: [`envelope_email_store::Draft::human_send_surface`]
///    is fail-closed on shape, a strict RFC 3339 timestamp, and binding to the
///    draft's *current* revision, and every content/attachment/metadata write —
///    plus Hold — strips the key outright.
///
/// The review attestation is required alongside it, so the row Tyler sent is
/// also a row he is recorded as having approved. Generic **Approve** records
/// only that attestation and never reaches this predicate: approving a draft is
/// not sending it.
///
/// Public because it is the whole definition of the sweep's one Governor
/// exception: any surface that queues a send (CLI `draft send`, MCP
/// `send_draft`) can — and does — assert that the row it produced does not
/// satisfy it.
pub fn dashboard_human_send_authorized(draft: &envelope_email_store::Draft) -> bool {
    draft.human_send_surface() == Some(DASHBOARD_SEND_SURFACE) && draft.human_approved()
}

/// Run the Governor gate for a due scheduled draft and record a sanitized audit
/// event. Returns the outcome; the sweep must refuse SMTP unless allowed.
///
/// This is the authoritative actual-send gate for queued/scheduled mail. It
/// re-derives fresh host facts from the persisted draft AND loads the durable
/// declaration the bot validated at queue time (`declared` + `require_declaration`,
/// computed by the caller from [`scheduled_attribution_inputs`]), then resolves
/// `declared ∪ derived` through [`gate_with_attribution`] — which refuses an
/// unattributed/invalid request BEFORE Governor is ever spawned. A bot-originated
/// draft with no valid current declaration therefore fails closed here even when
/// the derived set is rich; host facts never substitute for the bot's attribution.
async fn run_governor_gate(
    state: &AppState,
    draft: &envelope_email_store::Draft,
    creds: &envelope_email_store::models::AccountWithCredentials,
    subject: &str,
    attachments: &[envelope_email_transport::smtp::Attachment],
    declared: &[String],
    require_declaration: bool,
) -> envelope_email_transport::outbound::GovernorOutcome {
    use envelope_email_transport::outbound::{
        GovernorConfig, GovernorRequest, SendSurface, gate_with_attribution,
    };

    // A pending send that Tyler queued himself through the dashboard's
    // Human-only Send route IS the send. Governor does not score, review, or park
    // it — that is what stranded operator-clicked mail as `pending_review` for
    // days.
    //
    // The authorization is the click, not the authorship: who typed the words
    // (agent, MCP, CLI, or Tyler) does not change the fact that a human read this
    // exact revision and chose to transmit it. Keying this on `require_declaration`
    // instead — which is false only for a `human:*`-authored row — is what made an
    // agent-drafted body unsendable from the dashboard.
    //
    // The exception is bound to the transition, not to approval: only the
    // dashboard send route mints the authorization, only as the queue transition
    // itself, and only for the revision the operator saw. An agent re-queue
    // (CLI `draft send`, MCP `send_draft`) strips it, an edit or Hold withdraws
    // it, and a generic Approve never creates one. Everything without it — bot,
    // CLI, MCP, scheduled, approved-but-agent-queued — falls through to the full
    // fail-closed gate below, declaration requirement intact.
    if dashboard_human_send_authorized(draft) {
        let outcome = envelope_email_transport::outbound::GovernorOutcome::human_dashboard_send();
        let event = envelope_email_store::Event {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: draft.account_id.clone(),
            event_type: "send.human_dashboard".to_string(),
            folder: "policy".to_string(),
            uid: None,
            message_id: None,
            from_addr: None,
            subject: None,
            snippet: None,
            payload: Some(
                serde_json::json!({
                    "draft_id": draft.id,
                    "surface": "dashboard",
                    "governor": "skipped",
                })
                .to_string(),
            ),
            idempotency_key: None,
            secure_pending: false,
            acked_at: Some(chrono::Utc::now().to_rfc3339()),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        {
            let db = state.db.lock().await;
            let _ = db.insert_event(&event);
        }
        return outcome;
    }

    let account_domain = account_domain_from_username(&creds.account.username);

    let attachment_sizes: Vec<(String, u64)> = attachments
        .iter()
        .map(|a| (a.content_type.clone(), a.data.len() as u64))
        .collect();

    // Re-derive the FINAL attributed context from the persisted draft just
    // before SMTP, and resolve it against the durable bot declaration. This is
    // the authoritative gate: recipients, attachments, and threading are read
    // from what will actually be transmitted.
    let ctx = scheduled_send_context(draft, account_domain, attachments);
    let req = GovernorRequest::from_context_with_declared(
        &draft.account_id,
        subject,
        SendSurface::Scheduled,
        Some(&draft.id),
        &attachment_sizes,
        &ctx,
        declared,
        require_declaration,
    );

    let config = GovernorConfig::from_env();
    let outcome = gate_with_attribution(&config, &req);

    // Record a sanitized audit event (no bodies, no full addresses, no bytes).
    let event_type = if outcome.allowed {
        "send_governor.allowed"
    } else if outcome.is_attribution_failure() {
        "send_governor.attribution_refused"
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

// ── Historical deep-link compatibility ───────────────────────────────

/// The `folder` query of a pre-1.0.11 message deep link. Absent (or blank) means
/// the link predates folder-aware deep links, when INBOX was the only mailbox
/// those links could name.
#[derive(serde::Deserialize)]
struct LegacyFolderQuery {
    #[serde(default)]
    folder: Option<String>,
}

/// Redirect `/accounts/{account}/messages/{uid}` to the surface that can act on
/// it: the draft review composer when the uid names a synced local draft, the
/// canonical reader route otherwise.
///
/// Axum hands over the account percent-decoded from the path and the folder
/// percent-decoded from the query; both are re-encoded by
/// [`ui_paths::message_dashboard_path`] on the way out, so a `/` or `?` inside
/// either value cannot forge an extra path segment or query parameter. A
/// non-numeric uid fails the extractor with 400 rather than being spliced into
/// the target.
///
/// A Drafts uid with no local draft row still redirects to the reader route
/// rather than 404ing — the frontend intercepts a drafts-classified folder there
/// and renders a draft card instead of the read-only reader.
async fn legacy_message_redirect(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path((account, uid)): axum::extract::Path<(String, u32)>,
    axum::extract::Query(query): axum::extract::Query<LegacyFolderQuery>,
) -> Redirect {
    let folder = query
        .folder
        .as_deref()
        .filter(|folder| !folder.is_empty())
        .unwrap_or("INBOX");

    if envelope_email_transport::provider::classify_folder(folder) == Some("drafts")
        && let Some(path) = draft_review_path_for_imap_uid(&state, &account, uid).await
    {
        return Redirect::permanent(&path);
    }

    Redirect::permanent(&ui_paths::message_dashboard_path(&account, folder, uid))
}

/// Resolve `{account, imap uid}` to the draft review path, the same way
/// [`handlers::drafts::show_by_imap_uid`] resolves it for the API. `None` when
/// the account or the draft is unknown, or the lookup errors — the caller then
/// falls back to a link that still resolves.
async fn draft_review_path_for_imap_uid(
    state: &AppState,
    account: &str,
    uid: u32,
) -> Option<String> {
    let db = state.db.lock().await;
    let account = match handlers::drafts::resolve_account(&db, account) {
        Ok(Some(account)) => account,
        Ok(None) => return None,
        Err(e) => {
            warn!("legacy draft deep link: account lookup failed: {e}");
            return None;
        }
    };
    match db.get_draft_by_imap_uid(&account.id, uid) {
        Ok(Some(draft)) => Some(ui_paths::draft_dashboard_path(&account.id, &draft.id)),
        Ok(None) => None,
        Err(e) => {
            warn!("legacy draft deep link: draft lookup failed: {e}");
            None
        }
    }
}

/// Redirect `/accounts/{account}/cockpit` to the global cockpit route. The
/// account is dropped because the SPA cockpit spans every account.
async fn legacy_cockpit_redirect() -> Redirect {
    Redirect::permanent("/cockpit")
}

/// Redirect `/accounts/{account}/rules` to the global rules route.
async fn legacy_rules_redirect() -> Redirect {
    Redirect::permanent("/rules")
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

    #[test]
    fn scheduled_send_uses_persisted_from_override() {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Bruno', 'bruno@spainexpat.com', 'spainexpat.com',
                         'smtp.spainexpat.com', 587, 'imap.spainexpat.com', 993, 'encrypted')",
                [],
            )
            .unwrap();
        let draft = db
            .create_draft(
                "acc1",
                "member@example.test",
                Some("Questionnaire"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("cli"),
            )
            .unwrap();
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({
                "from": "SpainExpat Plus Ultra <plusultra@spainexpat.com>"
            }),
        )
        .unwrap();
        let stored = db.get_draft(&draft.id).unwrap().unwrap();

        assert_eq!(
            draft_from_override(&stored),
            Some("SpainExpat Plus Ultra <plusultra@spainexpat.com>")
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
    async fn address_history_backfill_populates_contacts_from_the_local_index() {
        use envelope_email_store::models::IndexedMessageInput;

        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Work', 'me@example.test', 'example.test',
                         'smtp.example.test', 587, 'imap.example.test', 993, 'x')",
                [],
            )
            .unwrap();
        db.upsert_indexed_message_summaries(
            "acc1",
            "INBOX",
            1,
            &[IndexedMessageInput {
                uid: 1,
                message_id: Some("<a@example.test>".into()),
                from_addr: "Ada Lovelace <ada@example.test>".into(),
                to_addr: "me@example.test".into(),
                subject: "Quarterly filing".into(),
                date: Some("Tue, 12 May 2026 12:00:00 +0000".into()),
                flags: Vec::new(),
                size: 10,
                snippet: None,
                thread_id: None,
            }],
        )
        .unwrap();

        // Years of correspondence already cached locally, none of it in the
        // dashboard's small INBOX snapshot.
        let thread = db
            .create_thread(
                "hearing",
                "2026-04-01T09:00:00Z",
                "2026-04-01T09:00:00Z",
                "acc1",
            )
            .unwrap();
        db.upsert_thread_message(
            &thread.thread_id,
            9,
            Some("<t9@court.test>"),
            None,
            None,
            "Sent",
            "me@example.test",
            "clerk@court.test",
            None,
            None,
            "2026-04-01T09:00:00Z",
            "Hearing date",
            true,
            None,
        )
        .unwrap();

        // Both caches predate the address book — exactly the upgrade case this
        // backfill exists for.
        assert!(db.list_contacts("acc1", None).unwrap().is_empty());

        let state = AppState::new(db, CredentialBackend::File);
        backfill_address_history(&state).await;

        let db = state.db.lock().await;
        let suggestions = db.suggest_addresses("acc1", "ada", 8).unwrap();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].email, "ada@example.test");
        assert_eq!(suggestions[0].name.as_deref(), Some("Ada Lovelace"));

        let from_threads = db.suggest_addresses("acc1", "clerk", 8).unwrap();
        assert_eq!(from_threads.len(), 1, "the thread cache must be backfilled");
        assert_eq!(from_threads[0].email, "clerk@court.test");

        // Backfill is a boundary, not a per-request cost: running it again
        // reads no thread rows and changes no counts.
        drop(db);
        backfill_address_history(&state).await;
        let db = state.db.lock().await;
        let again = db.reconcile_address_history("acc1").unwrap();
        assert_eq!(again.thread_rows, 0);
        // `history_count` is the derived counter this backfill owns;
        // `message_count` belongs to `envelope contacts add|import` and stays
        // untouched for a contact the backfill invented.
        let clerk: (i64, i64) = db
            .conn()
            .query_row(
                "SELECT history_count, message_count FROM contacts
                 WHERE account_id = 'acc1' AND email = 'clerk@court.test'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(clerk, (1, 0));
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

    /// Replicate the exact attribution composition `run_governor_gate` performs
    /// (context + persisted-declaration inputs + attributed gate), pointed at a
    /// nonexistent Governor binary in required mode so the outcome is decided
    /// entirely by the pre-spawn attribution logic.
    fn scheduled_gate_outcome(
        draft: &envelope_email_store::Draft,
    ) -> envelope_email_transport::outbound::GovernorOutcome {
        scheduled_gate_outcome_mode(
            draft,
            envelope_email_transport::outbound::GovernorMode::Required,
        )
    }

    fn scheduled_gate_outcome_mode(
        draft: &envelope_email_store::Draft,
        mode: envelope_email_transport::outbound::GovernorMode,
    ) -> envelope_email_transport::outbound::GovernorOutcome {
        use envelope_email_transport::attribution_persist::{
            PersistedDeclaration, scheduled_attribution_inputs,
        };
        use envelope_email_transport::outbound::{
            GovernorConfig, GovernorRequest, SendSurface, gate_with_attribution,
        };
        let persisted = PersistedDeclaration::from_metadata(draft.metadata.as_ref());
        let (declared, require) = scheduled_attribution_inputs(
            draft.created_by.as_deref(),
            draft.human_approved(),
            persisted.as_ref(),
            draft.revision,
        );
        let ctx = scheduled_send_context(draft, Some("example.com".to_string()), &[]);
        let req = GovernorRequest::from_context_with_declared(
            &draft.account_id,
            draft.subject.as_deref().unwrap_or(""),
            SendSurface::Scheduled,
            Some(&draft.id),
            &[],
            &ctx,
            &declared,
            require,
        );
        let config = GovernorConfig {
            mode,
            bin: "/nonexistent/governor-binary-xyz".to_string(),
        };
        gate_with_attribution(&config, &req)
    }

    fn sweep_test_db() -> Database {
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
        db
    }

    #[test]
    fn scheduled_bot_draft_without_declaration_fails_closed_not_governor_unavailable() {
        // A bot-originated queued draft (created_by=agent) with NO persisted
        // declaration but a rich external context must be refused with
        // attributes_required BEFORE Governor is spawned — a nonexistent binary
        // would otherwise yield governor_unavailable. Host facts never substitute.
        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("Quarterly numbers"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(!fetched.human_approved());

        let outcome = scheduled_gate_outcome(&fetched);
        assert!(!outcome.allowed);
        assert!(
            outcome.is_attribution_failure(),
            "bot draft with no declaration must fail attribution, not spawn Governor"
        );
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_required"));
        assert_ne!(outcome.decision, "unavailable");
    }

    #[test]
    fn scheduled_bot_draft_with_valid_declaration_reaches_governor() {
        // With a valid current bot declaration the sweep resolves attributed and
        // actually spawns Governor — a missing binary is governor_unavailable,
        // which is an operator failure, NOT an attribution failure.
        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("S"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.set_draft_attribution(
            &draft.id,
            &envelope_email_transport::attribution_persist::PersistedDeclaration::new_bot(
                &["financial_content".to_string()],
                0,
            )
            .to_value(),
        )
        .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();

        let outcome = scheduled_gate_outcome(&fetched);
        assert!(!outcome.allowed);
        assert!(
            !outcome.is_attribution_failure(),
            "a validly-declared draft reaches Governor; the failure is operator-side"
        );
        assert_eq!(outcome.block_code.as_deref(), Some("governor_unavailable"));
    }

    #[test]
    fn scheduled_bot_draft_human_approved_without_declaration_fails_closed() {
        // THE mandatory-declaration invariant at the attribution layer: for a
        // bot-originated draft (created_by=agent), a human approval SUPPLEMENTS
        // the bot's factual declaration (adding tyler_approved) — it never erases
        // the bot's attribution responsibility. With no persisted declaration the
        // resolution fails closed with attributes_required BEFORE Governor is
        // spawned; host facts never substitute for the missing declaration.
        //
        // This is the layer, not the dashboard send path: a current dashboard
        // Human-only Send attestation short-circuits `run_governor_gate` before it
        // ever resolves attribution (see
        // `dashboard_human_send_on_an_agent_drafted_body_skips_the_governor`).
        // Everything reaching this resolution — CLI, MCP, scheduled, unattested
        // dashboard rows — still owes its declaration.
        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("S"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        db.record_draft_human_approval(&draft.id, rev, "human:dashboard", "2026-08-08T09:00:00Z")
            .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(fetched.human_approved(), "the human attestation is present");

        let outcome = scheduled_gate_outcome(&fetched);
        assert!(!outcome.allowed);
        assert!(
            outcome.is_attribution_failure(),
            "a bot draft with no declaration fails closed even after human approval"
        );
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_required"));
        assert_ne!(
            outcome.decision, "unavailable",
            "Governor must never be spawned for an unattributed bot draft"
        );
    }

    /// A draft the operator composed on the dashboard (or the Tauri shell, which
    /// posts to the same API and is stamped `human:dashboard` by `compose.rs`) and
    /// then attested needs no bot declaration at the attribution layer either, so
    /// it resolves without one even before the gate's Human-only Send skip.
    #[test]
    fn scheduled_attribution_inputs_lift_the_declaration_only_for_human_origin() {
        use envelope_email_transport::attribution_persist::scheduled_attribution_inputs;

        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("S"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("human:dashboard"),
            )
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        db.record_draft_human_approval(&draft.id, rev, "human:dashboard", "2026-08-18T09:00:00Z")
            .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(fetched.human_approved(), "the human attestation is present");

        let (_declared, require) = scheduled_attribution_inputs(
            fetched.created_by.as_deref(),
            fetched.human_approved(),
            None,
            fetched.revision,
        );
        assert!(
            !require,
            "a human-composed, human-attested draft needs no bot declaration"
        );
        // Contrast: the same attestation on an agent-drafted row still owes its
        // declaration at THIS layer. The gate's Human-only Send skip is a separate
        // rule that runs first — see
        // `dashboard_human_send_on_an_agent_drafted_body_skips_the_governor`.
        let bot = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("S"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        let bot_rev = db.get_draft(&bot.id).unwrap().unwrap().revision;
        db.record_draft_human_approval(&bot.id, bot_rev, "human:dashboard", "2026-08-18T09:00:00Z")
            .unwrap();
        let bot = db.get_draft(&bot.id).unwrap().unwrap();
        let (_d, bot_require) = scheduled_attribution_inputs(
            bot.created_by.as_deref(),
            bot.human_approved(),
            None,
            bot.revision,
        );
        assert!(
            bot_require,
            "an agent-drafted row still requires its declaration after human approval"
        );
    }

    fn sweep_state() -> AppState {
        AppState::new(sweep_test_db(), CredentialBackend::File)
    }

    /// Credentials shaped for the gate, which reads only the account username
    /// (for the mail domain). No credential store is touched.
    async fn sweep_creds(state: &AppState) -> envelope_email_store::models::AccountWithCredentials {
        let db = state.db.lock().await;
        let account = db.get_account("acc1").unwrap().unwrap();
        envelope_email_store::models::AccountWithCredentials {
            account,
            password: "unused".to_string(),
            smtp_password: None,
            imap_password: None,
        }
    }

    /// Drive the REAL [`run_governor_gate`] the sweep calls, with the sweep's own
    /// attribution inputs, and return the outcome plus every policy event type it
    /// recorded. The event list is what proves whether Governor decided the send:
    /// the human path records `send.human_dashboard`, the governed path records a
    /// `send_governor.*` decision.
    async fn gate_with_events(
        state: &AppState,
        draft: &envelope_email_store::Draft,
    ) -> (
        envelope_email_transport::outbound::GovernorOutcome,
        Vec<String>,
    ) {
        use envelope_email_transport::attribution_persist::{
            PersistedDeclaration, scheduled_attribution_inputs,
        };
        let creds = sweep_creds(state).await;
        let persisted = PersistedDeclaration::from_metadata(draft.metadata.as_ref());
        let (declared, require) = scheduled_attribution_inputs(
            draft.created_by.as_deref(),
            draft.human_approved(),
            persisted.as_ref(),
            draft.revision,
        );
        let outcome = run_governor_gate(
            state,
            draft,
            &creds,
            draft.subject.as_deref().unwrap_or(""),
            &[],
            &declared,
            require,
        )
        .await;
        let events = {
            let db = state.db.lock().await;
            db.list_events(Some("acc1"), 50)
                .unwrap()
                .into_iter()
                .map(|e| e.event_type)
                .collect()
        };
        (outcome, events)
    }

    fn governor_decided(events: &[String]) -> bool {
        events.iter().any(|t| t.starts_with("send_governor."))
    }

    fn human_send_recorded(events: &[String]) -> bool {
        events.iter().any(|t| t == "send.human_dashboard")
    }

    /// Seed an unqueued, unattested draft with the given `created_by`.
    async fn seed_gate_draft(state: &AppState, created_by: &str) -> envelope_email_store::Draft {
        let db = state.db.lock().await;
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("Service dog request"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some(created_by),
            )
            .unwrap();
        db.get_draft(&draft.id).unwrap().unwrap()
    }

    async fn reload_draft(state: &AppState, draft_id: &str) -> envelope_email_store::Draft {
        let db = state.db.lock().await;
        db.get_draft(draft_id).unwrap().unwrap()
    }

    /// Click **Human-only Send** through the REAL dashboard route
    /// ([`handlers::drafts::send`]) on the revision the operator is looking at,
    /// and return the queued row. Nothing about the send authorization is
    /// hand-written here: whatever provenance the sweep later honors has to be
    /// something this handler actually minted.
    async fn dashboard_human_send(state: &AppState, draft_id: &str) -> envelope_email_store::Draft {
        use axum::extract::{Path, State};
        use axum::response::IntoResponse;
        let revision = reload_draft(state, draft_id).await.revision;
        let response = handlers::drafts::send(
            State(state.clone()),
            Path(("acc1".to_string(), draft_id.to_string())),
            axum::Json(handlers::drafts::DraftSendRequest {
                confirm: true,
                expected_revision: revision,
                cooldown_seconds: None,
                send_now: false,
            }),
        )
        .await
        .into_response();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::OK,
            "Human-only Send should have queued the draft"
        );
        reload_draft(state, draft_id).await
    }

    /// Click **Approve** through the REAL dashboard route
    /// ([`handlers::drafts::approve`]): a review decision on that revision, with
    /// no send and no queue transition.
    async fn dashboard_approve(state: &AppState, draft_id: &str) -> envelope_email_store::Draft {
        use axum::extract::{Path, State};
        use axum::response::IntoResponse;
        let revision = reload_draft(state, draft_id).await.revision;
        let response = handlers::drafts::approve(
            State(state.clone()),
            Path(("acc1".to_string(), draft_id.to_string())),
            axum::Json(handlers::drafts::DraftApproveRequest {
                expected_revision: revision,
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK, "approve");
        let approved = reload_draft(state, draft_id).await;
        assert!(
            approved.human_approved(),
            "Approve records the review attestation"
        );
        assert!(
            approved.send_after.is_none(),
            "Approve must not queue a send"
        );
        approved
    }

    /// Queue a draft exactly the way an agent does — the store primitive both
    /// CLI `draft send` and MCP `send_draft` reach through
    /// (`commands::drafts::queue_bot_draft_for_send`), with the same persisted
    /// bot declaration bound to the same revision.
    async fn agent_queue_for_send(state: &AppState, draft_id: &str) -> envelope_email_store::Draft {
        use envelope_email_transport::attribution_persist::PersistedDeclaration;
        let db = state.db.lock().await;
        let rev = db.get_draft(draft_id).unwrap().unwrap().revision;
        let attribution =
            PersistedDeclaration::new_bot(&["recipient_requested".to_string()], rev).to_value();
        db.queue_draft_for_send(draft_id, rev, "2000-01-01T00:00:00Z", &attribution)
            .unwrap();
        db.get_draft(draft_id).unwrap().unwrap()
    }

    #[tokio::test]
    async fn dashboard_approval_never_authorizes_a_later_agent_send() {
        // Generic dashboard Approve reviews a draft; it does not send it and it
        // does not hand the agent an ungoverned send. An agent that queues the
        // approved row afterwards (CLI `draft send` / MCP `send_draft`) is still
        // fully governed — otherwise "approve this draft" would silently mean
        // "and let the bot transmit it unscored whenever it likes".
        let state = sweep_state();
        let draft = seed_gate_draft(&state, "agent").await;
        dashboard_approve(&state, &draft.id).await;

        let queued = agent_queue_for_send(&state, &draft.id).await;

        let (outcome, events) = gate_with_events(&state, &queued).await;

        assert_ne!(
            outcome.decision, "human_dashboard",
            "an agent-queued send is never a human send, approved or not"
        );
        assert!(governor_decided(&events), "the gate ran: {events:?}");
        assert!(!human_send_recorded(&events), "{events:?}");

        // And it reaches SMTP only if Governor allows: against a pinned
        // required-mode config (rather than the host's Governor environment) the
        // send is refused outright.
        assert!(
            !scheduled_gate_outcome(&queued).allowed,
            "no SMTP without a Governor allow"
        );
    }

    #[tokio::test]
    async fn dashboard_human_send_on_an_agent_drafted_body_skips_the_governor() {
        // THE live incident: an agent wrote the body, Tyler clicked Human-only
        // Send on the dashboard. That click IS the send — Governor does not
        // re-score it, does not park it as `pending_review`, and records no
        // Governor decision. The click is the authorization, whatever wrote the
        // words.
        let state = sweep_state();
        let draft = seed_gate_draft(&state, "agent").await;
        let queued = dashboard_human_send(&state, &draft.id).await;
        assert!(
            queued.send_after.is_some(),
            "the click queues through the outbox cooldown rather than sending inline"
        );
        assert_eq!(
            queued.status,
            envelope_email_store::models::DraftStatus::Draft,
            "the sweep, not the handler, transmits it"
        );

        let (outcome, events) = gate_with_events(&state, &queued).await;

        assert_eq!(
            outcome,
            envelope_email_transport::outbound::GovernorOutcome::human_dashboard_send(),
            "a dashboard Human-only Send is a human send, not a scored one"
        );
        assert!(outcome.allowed);
        assert!(
            human_send_recorded(&events),
            "the human send must be audited: {events:?}"
        );
        assert!(
            !governor_decided(&events),
            "Governor must not decide a dashboard Human-only Send: {events:?}"
        );

        // Hold is untouched by the send authorization: it takes the queued
        // message back out of the outbox and leaves it editable.
        let held = {
            let db = state.db.lock().await;
            db.hold_scheduled_draft(&queued.id).unwrap()
        };
        assert!(held.send_after.is_none(), "Hold cleared the schedule");
        assert_eq!(
            held.status,
            envelope_email_store::models::DraftStatus::Draft
        );
    }

    #[tokio::test]
    async fn agent_draft_without_a_dashboard_send_stays_governed() {
        // The bot/CLI/MCP/scheduled path is untouched: with no dashboard
        // Human-only Send behind it the gate runs, records a Governor decision,
        // and the factual-declaration requirement still holds.
        let state = sweep_state();
        let draft = seed_gate_draft(&state, "agent").await;
        assert!(!draft.human_approved());

        let (outcome, events) = gate_with_events(&state, &draft).await;

        assert_ne!(
            outcome.decision, "human_dashboard",
            "an unattested agent draft must never take the human-send path"
        );
        assert!(governor_decided(&events), "the gate ran: {events:?}");
        assert!(!human_send_recorded(&events), "{events:?}");

        // The attribution requirement, proven deterministically against a pinned
        // required-mode config rather than the host's Governor environment.
        let refused = scheduled_gate_outcome(&draft);
        assert!(!refused.allowed);
        assert!(refused.is_attribution_failure());
        assert_eq!(refused.block_code.as_deref(), Some("attributes_required"));
    }

    #[tokio::test]
    async fn an_edit_after_the_click_invalidates_the_send_authorization_and_re_governs() {
        // The send authorization is revision-bound: editing the body after
        // clicking Human-only Send bumps the revision and strips it, so the next
        // send is governed again. A stale click is never a skip.
        let state = sweep_state();
        let draft = seed_gate_draft(&state, "agent").await;
        let queued = dashboard_human_send(&state, &draft.id).await;
        let edited = {
            let db = state.db.lock().await;
            db.update_draft_content(
                &queued.id,
                None,
                None,
                None,
                None,
                Some("edited body"),
                None,
            )
            .unwrap();
            db.get_draft(&queued.id).unwrap().unwrap()
        };
        assert_ne!(edited.revision, queued.revision);

        let (outcome, events) = gate_with_events(&state, &edited).await;

        assert_ne!(
            outcome.decision, "human_dashboard",
            "an edited draft is not the version the human sent"
        );
        assert!(governor_decided(&events), "{events:?}");
        assert!(!human_send_recorded(&events), "{events:?}");
    }

    #[tokio::test]
    async fn hold_then_agent_requeue_is_governed_until_the_operator_clicks_again() {
        // Hold withdraws the send: it takes the message out of the outbox, so the
        // authorization that queued it dies with the schedule. An agent that
        // re-queues the held draft gets a fully governed send, and only another
        // Human-only Send restores the human path.
        let state = sweep_state();
        let draft = seed_gate_draft(&state, "agent").await;
        dashboard_human_send(&state, &draft.id).await;
        {
            let db = state.db.lock().await;
            db.hold_scheduled_draft(&draft.id).unwrap();
        }

        let requeued = agent_queue_for_send(&state, &draft.id).await;
        let (outcome, events) = gate_with_events(&state, &requeued).await;
        assert_ne!(
            outcome.decision, "human_dashboard",
            "a held-then-agent-requeued send is the agent's send"
        );
        assert!(governor_decided(&events), "{events:?}");
        assert!(!human_send_recorded(&events), "{events:?}");

        // The operator clicking again re-authorizes it.
        let reclicked = dashboard_human_send(&state, &draft.id).await;
        let (outcome, events) = gate_with_events(&state, &reclicked).await;
        assert_eq!(outcome.decision, "human_dashboard");
        assert!(human_send_recorded(&events), "{events:?}");
    }

    #[tokio::test]
    async fn human_composed_dashboard_send_still_skips_the_gate() {
        // Unchanged behavior for the draft the operator composed AND sent from
        // the dashboard — the case that already worked.
        let state = sweep_state();
        let draft = seed_gate_draft(&state, "human:dashboard").await;
        let queued = dashboard_human_send(&state, &draft.id).await;

        let (outcome, events) = gate_with_events(&state, &queued).await;

        assert_eq!(
            outcome,
            envelope_email_transport::outbound::GovernorOutcome::human_dashboard_send()
        );
        assert!(human_send_recorded(&events), "{events:?}");
        assert!(!governor_decided(&events), "{events:?}");
    }

    #[test]
    fn scheduled_bot_draft_human_approved_with_declaration_carries_both_sets() {
        // Bot origin + human approval + a valid bot declaration: the send is
        // attributed and reaches Governor, and the resolved set carries BOTH the
        // bot's declared fact AND the host-attested tyler_approved. Approval
        // supplements the declaration; it does not replace it.
        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("S"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.set_draft_attribution(
            &draft.id,
            &envelope_email_transport::attribution_persist::PersistedDeclaration::new_bot(
                &["financial_content".to_string()],
                0,
            )
            .to_value(),
        )
        .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        db.record_draft_human_approval(&draft.id, rev, "human:dashboard", "2026-08-08T09:00:00Z")
            .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(fetched.human_approved());

        let outcome = scheduled_gate_outcome(&fetched);
        assert!(
            !outcome.is_attribution_failure(),
            "a validly-declared bot draft reaches Governor"
        );
        assert_eq!(outcome.block_code.as_deref(), Some("governor_unavailable"));
        let resolution = outcome.resolution.expect("attributed resolution present");
        assert!(
            resolution
                .declared_attrs
                .contains(&"financial_content".to_string()),
            "the bot declaration is preserved"
        );
        assert!(
            resolution
                .governor_attrs
                .iter()
                .any(|a| a == "tyler_approved"),
            "human approval adds tyler_approved on top of the declaration"
        );
        assert!(
            resolution
                .governor_attrs
                .contains(&"financial_content".to_string()),
            "the declared fact reaches Governor alongside the attestation"
        );
    }

    #[test]
    fn scheduled_warn_mode_bot_draft_without_declaration_still_fails_closed() {
        // Warn mode does NOT waive the attribution precondition at the sweep: a
        // bot draft with no declaration is refused with attributes_required and
        // Governor is never spawned, exactly as in required mode. Warn only
        // softens a Governor VERDICT on an already-attributed send.
        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("S"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();

        let outcome = scheduled_gate_outcome_mode(
            &fetched,
            envelope_email_transport::outbound::GovernorMode::Warn,
        );
        assert!(!outcome.allowed, "warn must fail closed at the sweep too");
        assert!(outcome.is_attribution_failure());
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_required"));
        assert_ne!(outcome.decision, "unavailable");
    }

    #[test]
    fn scheduled_warn_mode_bot_draft_with_invalid_declaration_fails_closed() {
        // A stale/invalid declaration in warn mode is still refused — never
        // allowed through as an unattributed send.
        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("S"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        // Persist a declaration keyed to a WRONG (stale) revision so it is dropped.
        db.set_draft_attribution(
            &draft.id,
            &envelope_email_transport::attribution_persist::PersistedDeclaration::new_bot(
                &["financial_content".to_string()],
                999,
            )
            .to_value(),
        )
        .unwrap();
        // set_draft_attribution re-stamps the revision from the row, so force a
        // genuine mismatch by bumping the row revision via a material body edit.
        db.update_draft_content(&draft.id, None, None, None, None, Some("edited body"), None)
            .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(
            fetched.metadata.is_some(),
            "the stale attribution block is still present but keyed to the old revision"
        );

        let outcome = scheduled_gate_outcome_mode(
            &fetched,
            envelope_email_transport::outbound::GovernorMode::Warn,
        );
        assert!(!outcome.allowed);
        assert!(outcome.is_attribution_failure());
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_required"));
    }

    #[test]
    fn scheduled_unknown_legacy_draft_fails_closed() {
        // A legacy/unknown-provenance scheduled draft (no created_by marker, no
        // persisted declaration, no human attestation) must be treated as
        // bot-originated and fail closed — never silently treated as human.
        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("S"),
                Some("body"),
                None,
                None,
                None,
                None,
                None, // unknown provenance
            )
            .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(!fetched.human_approved());

        let outcome = scheduled_gate_outcome(&fetched);
        assert!(!outcome.allowed);
        assert!(outcome.is_attribution_failure());
        assert_eq!(outcome.block_code.as_deref(), Some("attributes_required"));
    }

    #[test]
    fn scheduled_human_attested_draft_does_not_require_a_bot_declaration() {
        // A revision-bound human attestation lifts the bot-declaration rule: the
        // sweep resolves attributed (tyler_approved derived) with no bot
        // declaration, and reaches Governor (unavailable here, never an
        // attribution failure). No bot declaration is fabricated.
        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("S"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("human:dashboard"),
            )
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        db.record_draft_human_approval(&draft.id, rev, "human:dashboard", "2026-08-08T09:00:00Z")
            .unwrap();
        let fetched = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(fetched.human_approved());

        let outcome = scheduled_gate_outcome(&fetched);
        assert!(!outcome.is_attribution_failure());
        assert_eq!(outcome.block_code.as_deref(), Some("governor_unavailable"));
        // The resolved set carried tyler_approved but never a fabricated bot decl.
        let resolution = outcome.resolution.expect("attributed resolution present");
        assert!(
            resolution
                .governor_attrs
                .iter()
                .any(|a| a == "tyler_approved")
        );
        assert!(
            resolution.declared_attrs.is_empty(),
            "no bot declaration fabricated"
        );
    }

    #[test]
    fn bot_draft_bounded_retry_then_parks_without_storm() {
        // End-to-end proof of the bounded attribution correction loop, driving the
        // exact sweep transitions against the real store: a bot draft that never
        // carries a valid declaration is retried at attempts 1 and 2, parked at
        // attempt 3 (pending_review, scheduling disabled, park_reason recorded),
        // and never selected as due again.
        use envelope_email_transport::attribution_persist::{
            AttributionFailureAction, PARK_REASON_ATTRIBUTION_EXHAUSTED,
            attribution_failure_action, scheduled_attribution_inputs, scheduled_origin,
        };

        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("S"),
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

        for expected in 1..=3u32 {
            // The draft must be due and claimable at the start of each sweep.
            let due = db.list_drafts_due_for_send().unwrap();
            assert_eq!(due.len(), 1, "attempt {expected}: draft should be due");
            let scanned = &due[0];
            let lease = db
                .claim_draft_for_sending(&scanned.id, scanned.revision)
                .unwrap()
                .expect("claim");
            let claimed = db.get_draft(&scanned.id).unwrap().unwrap();

            // The gate fails closed (attributes_required) — Governor never spawns.
            let outcome = scheduled_gate_outcome(&claimed);
            assert!(outcome.is_attribution_failure());

            // Drive the exact decision the sweep now makes, from durable origin.
            let persisted =
                envelope_email_transport::attribution_persist::PersistedDeclaration::from_metadata(
                    claimed.metadata.as_ref(),
                );
            let (declared, _require) = scheduled_attribution_inputs(
                claimed.created_by.as_deref(),
                claimed.human_approved(),
                persisted.as_ref(),
                claimed.revision,
            );
            let prior = persisted
                .as_ref()
                .filter(|d| d.is_current(claimed.revision))
                .map(|d| d.attempts)
                .unwrap_or(0);
            let origin = scheduled_origin(
                claimed.created_by.as_deref(),
                persisted.as_ref(),
                claimed.revision,
            );
            let action = attribution_failure_action(origin, &declared, claimed.revision, prior);
            match action {
                AttributionFailureAction::Park { value } => {
                    assert_eq!(expected, 3, "bot draft parks at attempt 3");
                    assert!(
                        db.park_attribution_exhausted(&claimed.id, &lease, &value)
                            .unwrap()
                    );
                }
                AttributionFailureAction::Retry { value } => {
                    assert!(expected < 3);
                    assert_eq!(value["origin"], "bot", "bot origin preserved");
                    assert!(
                        db.defer_attribution_retry(&claimed.id, &lease, &value)
                            .unwrap()
                    );
                }
                AttributionFailureAction::HumanReview => {
                    panic!("bot draft must not take the human path")
                }
            }
        }

        // Attempt 3 parked it: pending_review, no scheduling, honest park_reason,
        // and never due again — no retry storm.
        let parked = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(
            parked.status,
            envelope_email_store::DraftStatus::PendingReview
        );
        assert_eq!(parked.send_after, None);
        let parked_attr = parked.metadata.unwrap()["attribution"].clone();
        assert_eq!(
            parked_attr["park_reason"],
            PARK_REASON_ATTRIBUTION_EXHAUSTED
        );
        assert_eq!(parked_attr["origin"], "bot");
        assert!(db.list_drafts_due_for_send().unwrap().is_empty());
    }

    #[test]
    fn human_queue_attribution_block_reflects_attestation_without_fabricating_bot() {
        // Block 7: a human-queued dashboard draft's additive success block carries
        // the durable human attestation (tyler_approved derived) with NO fabricated
        // bot declaration, and defers the Governor decision to the sweep.
        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Hi"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("human:dashboard"),
            )
            .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        db.record_draft_human_approval(&draft.id, rev, "human:dashboard", "2026-08-08T09:00:00Z")
            .unwrap();
        let attested = db.get_draft(&draft.id).unwrap().unwrap();

        // The account domain is derived from the username, exactly as the sweep
        // does (`agent@example.com` → `example.com`).
        let block = human_queue_attribution_block(&attested, "agent@example.com");
        assert_eq!(block["attribution_state"], "attributed");
        assert!(
            block["declared_attrs"].as_array().unwrap().is_empty(),
            "no fabricated bot declaration"
        );
        assert!(
            block["derived_attrs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a == "tyler_approved"),
            "the durable human attestation is derived: {block}"
        );
        assert_eq!(block["governor"], serde_json::Value::Null);
        assert!(block.get("governor_decision_pending").is_some());
        let text = block.to_string();
        for banned in ["\"score\"", "weight", "threshold"] {
            assert!(!text.contains(banned), "block leaked {banned}");
        }
    }

    #[tokio::test]
    async fn human_queue_attribution_block_matches_the_gate_for_an_agent_drafted_body() {
        // The advertised block and the sweep must agree. A dashboard Human-only
        // Send on an agent-written body IS a human send, so the queue response
        // must not advertise `attributes_required` for a message the sweep will
        // transmit — that self-contradiction is what the caller reads.
        let state = sweep_state();
        let draft = seed_gate_draft(&state, "agent").await;
        let queued = dashboard_human_send(&state, &draft.id).await;

        let block = human_queue_attribution_block(&queued, "agent@example.com");
        assert_eq!(
            block["attribution_state"], "attributed",
            "the sweep will send this; the block must not claim otherwise: {block}"
        );
        assert!(
            block["derived_attrs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a == "tyler_approved"),
            "the human attestation is the authorization: {block}"
        );
        assert!(
            block["declared_attrs"].as_array().unwrap().is_empty(),
            "no bot declaration fabricated: {block}"
        );
    }

    /// An attachment-bearing human-queued draft must advertise the real
    /// attachment host facts (`has_attachment`, and `sensitive_attachment` for a
    /// sensitive filename) — derived from the draft's own snapshots — so the
    /// advertised block agrees with what the sweep will derive, instead of
    /// omitting them by resolving against an empty attachment list.
    #[test]
    fn human_queue_attribution_block_reflects_real_attachment_facts() {
        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "buyer@example.com",
                Some("Invoice"),
                Some("Body"),
                None,
                None,
                None,
                None,
                Some("human:dashboard"),
            )
            .unwrap();
        // A sensitive attachment snapshot (filename/content_type only — bytes are
        // irrelevant to the derived facts).
        db.update_draft_attachments(
            &draft.id,
            &[serde_json::json!({
                "filename": "statement.pdf",
                "content_type": "application/pdf",
                "size": 3,
                "data_base64": "AAAA",
            })],
        )
        .unwrap();
        let rev = db.get_draft(&draft.id).unwrap().unwrap().revision;
        db.record_draft_human_approval(&draft.id, rev, "human:dashboard", "2026-08-08T09:00:00Z")
            .unwrap();
        let attested = db.get_draft(&draft.id).unwrap().unwrap();

        let block = human_queue_attribution_block(&attested, "agent@example.com");
        let derived: Vec<&str> = block["derived_attrs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a.as_str())
            .collect();
        assert!(
            derived.contains(&"has_attachment"),
            "attachment fact must be derived from the snapshot, not omitted: {block}"
        );
        assert!(
            derived.contains(&"sensitive_attachment"),
            "a sensitive attachment must be classified from the snapshot: {block}"
        );
    }

    #[test]
    fn human_origin_attribution_failure_parks_for_reapproval_without_bot_fabrication() {
        // A genuinely human-originated draft (created_by=human:*) whose approval is
        // stale/missing fails the attribution precondition at the sweep. It must be
        // parked for honest human re-approval WITHOUT fabricating a bot
        // declaration — its human origin survives so a fresh attestation recovers it.
        use envelope_email_transport::attribution_persist::{
            AttributionFailureAction, DeclarationOrigin, PersistedDeclaration, ScheduledOrigin,
            attribution_failure_action, scheduled_attribution_inputs, scheduled_origin,
        };

        let db = sweep_test_db();
        let draft = db
            .create_draft(
                "acc1",
                "external@other.example",
                Some("S"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("human:dashboard"),
            )
            .unwrap();
        db.update_draft_send_after(&draft.id, "2000-01-01T00:00:00Z")
            .unwrap();

        let due = db.list_drafts_due_for_send().unwrap();
        let scanned = &due[0];
        let lease = db
            .claim_draft_for_sending(&scanned.id, scanned.revision)
            .unwrap()
            .expect("claim");
        let claimed = db.get_draft(&scanned.id).unwrap().unwrap();
        assert!(!claimed.human_approved(), "no current attestation");

        let outcome = scheduled_gate_outcome(&claimed);
        assert!(outcome.is_attribution_failure());

        let persisted = PersistedDeclaration::from_metadata(claimed.metadata.as_ref());
        let (declared, require) = scheduled_attribution_inputs(
            claimed.created_by.as_deref(),
            claimed.human_approved(),
            persisted.as_ref(),
            claimed.revision,
        );
        assert!(
            require,
            "a stale human approval still requires re-attestation"
        );
        let origin = scheduled_origin(
            claimed.created_by.as_deref(),
            persisted.as_ref(),
            claimed.revision,
        );
        assert_eq!(origin, ScheduledOrigin::Human);
        let action = attribution_failure_action(origin, &declared, claimed.revision, 0);
        assert_eq!(action, AttributionFailureAction::HumanReview);

        // The sweep's HumanReview transition: release to pending_review only.
        assert!(
            db.release_sending_draft(
                &claimed.id,
                &lease,
                envelope_email_store::DraftStatus::PendingReview
            )
            .unwrap()
        );
        let parked = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(
            parked.status,
            envelope_email_store::DraftStatus::PendingReview
        );
        // NO fabricated bot declaration was written.
        let after = PersistedDeclaration::from_metadata(parked.metadata.as_ref());
        assert!(
            after.as_ref().map(|d| d.origin) != Some(DeclarationOrigin::Bot),
            "human draft must never be rewritten as bot-originated"
        );
        // Recoverable: still human-originated for a fresh attestation, and not due.
        assert_eq!(
            scheduled_origin(
                parked.created_by.as_deref(),
                after.as_ref(),
                parked.revision
            ),
            ScheduledOrigin::Human
        );
        assert!(db.list_drafts_due_for_send().unwrap().is_empty());
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
            review_ticket_id: None,
            block_code: block_code.map(str::to_string),
            block_reason: Some("blocked".to_string()),
            route: None,
            resolution: None,
            suggestions: Vec::new(),
            surface: None,
            action_echo: None,
            parked: false,
            parked_draft_id: None,
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

    #[test]
    fn scheduled_send_context_derives_short_body_for_every_body_shape() {
        // Finding 1 (scheduled boundary): the sweep derives `short_body` from the
        // FINAL persisted bodies for every shape — text, HTML-only, dual, empty —
        // via the one canonical policy, so a truthful declaration is corroborated
        // identically to the direct CLI/MCP boundary and is always observable.
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
        let long_words = vec!["word"; 120].join(" ");
        let long_html = format!("<p>{}</p>", vec!["word"; 120].join(" "));

        let short_for = |text: Option<&str>, html: Option<&str>| {
            let draft = db
                .create_draft(
                    "acc1",
                    "to@ex.com",
                    Some("S"),
                    text,
                    html,
                    None,
                    None,
                    None,
                    Some("agent"),
                )
                .unwrap();
            scheduled_send_context(&draft, Some("martin.fm".to_string()), &[]).short_body
        };

        assert_eq!(
            short_for(Some("just a few words"), None),
            Some(true),
            "text short"
        );
        assert_eq!(short_for(Some(&long_words), None), Some(false), "text long");
        assert_eq!(
            short_for(None, Some("<html><body><p>tiny note</p></body></html>")),
            Some(true),
            "html-only short"
        );
        assert_eq!(
            short_for(None, Some(&long_html)),
            Some(false),
            "html-only long"
        );
        assert_eq!(
            short_for(Some("short text alt"), Some(&long_html)),
            Some(true),
            "dual: text alternative canonical"
        );
        // Empty body: zero words → short, and still observable (never unknown).
        assert_eq!(short_for(Some(""), None), Some(true), "empty body");
    }

    #[test]
    fn scheduled_sweep_persists_sent_proof_after_resolving_it() {
        // Finding 3 (scheduled side of direct/scheduled durable parity): the sweep
        // resolves the Sent copy, then annotates the durable draft row with the
        // dedicated folder-qualified proof. Full send needs live SMTP/IMAP, so
        // guard the wiring/ordering at the source boundary.
        let src = include_str!("lib.rs");
        let fn_start = src
            .find("async fn resolve_and_record_sent_copy")
            .expect("scheduled sent-copy resolver present");
        let body = &src[fn_start..];
        let resolve_at = body
            .find("resolve_sent_copy_after_send(")
            .expect("scheduled path resolves the Sent copy");
        let record_at = body
            .find("record_sent_copy_proof(")
            .expect("scheduled path records the Sent proof durably");
        assert!(
            record_at > resolve_at,
            "must resolve the Sent copy before persisting the proof"
        );
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
            false,
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
            db.queue_draft_with_human_send(
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

    #[test]
    fn transition_outcome_never_claims_an_unpersisted_transition() {
        // Persisted → the intended label; failed persistence → the truthful
        // inert diagnostic, never a false blocked/deferred.
        assert_eq!(transition_outcome(true, "blocked"), "blocked");
        assert_eq!(transition_outcome(true, "deferred"), "deferred");
        assert_eq!(transition_outcome(false, "blocked"), "transition_failed");
        assert_eq!(transition_outcome(false, "deferred"), "transition_failed");
    }

    /// Owner-token mismatch (a concurrent transition stole/changed the lease):
    /// every claim-transition helper the sweep uses must return `false`, the
    /// published outcome must be the truthful `transition_failed` (never a false
    /// `blocked`/`deferred`/`parked`), and the row must stay inert as `sending`
    /// — still due, never marked sent, so no send happens on this pass.
    #[test]
    fn owner_token_mismatch_publishes_transition_failed_and_sends_nothing() {
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
        let _real_lease = db
            .claim_draft_for_sending(&draft.id, draft.revision)
            .unwrap()
            .expect("due draft is claimed before SMTP");

        // A different (stale/stolen) lease: every transition helper refuses.
        let wrong = "not-the-owner-lease";
        let attr = serde_json::json!({ "declared_attrs": [], "attempts": 3 });
        assert!(
            !db.defer_attribution_retry(&draft.id, wrong, &attr).unwrap(),
            "defer under a non-owner lease must not persist"
        );
        assert!(
            !db.park_attribution_exhausted(&draft.id, wrong, &attr)
                .unwrap(),
            "park under a non-owner lease must not persist"
        );
        assert!(
            !db.release_sending_draft(&draft.id, wrong, DraftStatus::PendingReview)
                .unwrap(),
            "release under a non-owner lease must not persist"
        );

        // Therefore the sweep publishes the truthful outcome, never a false one.
        assert_eq!(transition_outcome(false, "blocked"), "transition_failed");
        assert_eq!(transition_outcome(false, "deferred"), "transition_failed");

        // The row is untouched: still claimed as `sending`, still scheduled,
        // never marked sent — so this pass transmitted nothing and parked nothing.
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(
            after.status,
            DraftStatus::Sending,
            "an owner-mismatch leaves the claim inert, not blocked/parked"
        );
        assert_eq!(
            after.send_after.as_deref(),
            Some("2000-01-01T00:00:00Z"),
            "no schedule change on a failed transition"
        );
        assert!(after.sent_at.is_none(), "nothing was sent");
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

    /// `Location` header of a response, as a string.
    fn location_of(response: &Response) -> &str {
        response
            .headers()
            .get(header::LOCATION)
            .expect("redirect response must carry a Location header")
            .to_str()
            .expect("Location is ASCII")
    }

    /// Historical `/accounts/<id>/messages/<uid>` links (emitted by every
    /// Envelope release through 1.0.10) have no client route in the v2 bundle and
    /// render the SvelteKit 404 page. Serving the SPA shell for them — which the
    /// old test asserted — is exactly the failure: HTTP 200 with a 404 rendered
    /// inside it. They must redirect to the canonical reader route instead.
    #[tokio::test]
    async fn legacy_message_deep_link_redirects_to_the_canonical_reader_route() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/accounts/109c5747-8498-4614-945a-837462ae0aaf/messages/33281\
                         ?folder=%5BGmail%5D%2FSent%20Mail",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            location_of(&response),
            "/mail/unified/109c5747-8498-4614-945a-837462ae0aaf/33281\
             ?folder=%5BGmail%5D%2FSent%20Mail"
        );
    }

    /// A Drafts-folder uid names a message the reader cannot edit or send. When
    /// a local draft row carries that uid, the legacy link must land on the
    /// review composer — the account resolves by id or by email, and every
    /// folder name `classify_folder` calls drafts takes the same path.
    #[tokio::test]
    async fn legacy_draft_deep_link_redirects_to_the_review_composer() {
        let (state, draft_id, _) = test_state();
        let app = dashboard_router(state);
        let expected = format!("/accounts/acc1/drafts/{draft_id}");

        for uri in [
            "/accounts/acc1/messages/38103?folder=Drafts",
            "/accounts/acc1/messages/38103?folder=%5BGmail%5D%2FDrafts",
            "/accounts/acc1/messages/38103?folder=INBOX.Drafts",
            "/accounts/editor%40spainexpat.com/messages/38103?folder=Drafts",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                StatusCode::PERMANENT_REDIRECT,
                "{uri} must permanently redirect"
            );
            assert_eq!(location_of(&response), expected, "{uri} target");
        }
    }

    /// A Drafts uid with no local draft row has no review surface to offer, and
    /// a 404 would be worse than the reader — the frontend intercepts the
    /// drafts folder there and renders a draft card. Drafts belonging to another
    /// account must never be handed over either.
    #[tokio::test]
    async fn legacy_draft_deep_link_without_a_local_draft_stays_resolvable() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);

        for (uri, expected) in [
            (
                "/accounts/acc1/messages/99999?folder=Drafts",
                "/mail/unified/acc1/99999?folder=Drafts",
            ),
            // acc2 has a draft, but not one synced to 38103.
            (
                "/accounts/acc2/messages/38103?folder=Drafts",
                "/mail/unified/acc2/38103?folder=Drafts",
            ),
            (
                "/accounts/unknown-account/messages/38103?folder=Drafts",
                "/mail/unified/unknown-account/38103?folder=Drafts",
            ),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                StatusCode::PERMANENT_REDIRECT,
                "{uri} must permanently redirect, not 404"
            );
            assert_eq!(location_of(&response), expected, "{uri} target");
        }
    }

    /// The draft lookup is scoped to drafts-classified folders. A uid that
    /// happens to match a synced draft's uid in any other mailbox is a different
    /// message and must keep going to the reader.
    #[tokio::test]
    async fn legacy_message_deep_link_keeps_the_reader_for_non_draft_folders() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);

        for (uri, expected) in [
            (
                "/accounts/acc1/messages/57?folder=INBOX",
                "/mail/unified/acc1/57?folder=INBOX",
            ),
            (
                "/accounts/acc1/messages/38103?folder=INBOX",
                "/mail/unified/acc1/38103?folder=INBOX",
            ),
            (
                "/accounts/acc1/messages/38103?folder=%5BGmail%5D%2FSent%20Mail",
                "/mail/unified/acc1/38103?folder=%5BGmail%5D%2FSent%20Mail",
            ),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT, "{uri}");
            assert_eq!(location_of(&response), expected, "{uri} target");
        }
    }

    /// A legacy link without a `folder` query predates folder-aware deep links;
    /// INBOX is the only defensible default and must be explicit in the target.
    #[tokio::test]
    async fn legacy_message_deep_link_without_folder_defaults_to_inbox() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/accounts/acc1/messages/57")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(location_of(&response), "/mail/unified/acc1/57?folder=INBOX");
    }

    /// A present-but-blank `folder=` deserializes to `Some("")`, which would
    /// otherwise emit `?folder=` and leave the reader with no mailbox to scope
    /// the uid to. It takes the same INBOX default as an absent query.
    #[tokio::test]
    async fn legacy_message_deep_link_with_blank_folder_defaults_to_inbox() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/accounts/acc1/messages/57?folder=")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(location_of(&response), "/mail/unified/acc1/57?folder=INBOX");
    }

    /// A uid that is not a number cannot name an IMAP message, so the extractor
    /// rejects it outright. Redirecting would mint a canonical-looking link that
    /// 404s one hop later, and the raw segment must never reach the target.
    #[tokio::test]
    async fn legacy_message_deep_link_rejects_a_nonnumeric_uid() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);

        for uri in [
            "/accounts/acc1/messages/not-a-uid",
            "/accounts/acc1/messages/-1?folder=INBOX",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{uri} must be rejected, not redirected"
            );
            assert!(
                response.headers().get(header::LOCATION).is_none(),
                "{uri} must not carry a Location header"
            );
        }
    }

    /// An account id with reserved characters arrives percent-encoded, is decoded
    /// by the router, and must be re-encoded into the redirect target — never
    /// spliced in raw, which would forge extra path segments or a query.
    #[tokio::test]
    async fn legacy_message_deep_link_reencodes_the_decoded_account() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/accounts/acct%2Fone%3Fx/messages/9?folder=Sent%20Items%20%26%20More")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            location_of(&response),
            "/mail/unified/acct%2Fone%3Fx/9?folder=Sent%20Items%20%26%20More"
        );
    }

    #[tokio::test]
    async fn legacy_account_cockpit_and_rules_links_redirect_to_global_routes() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);

        for (uri, expected) in [
            ("/accounts/acc1/cockpit", "/cockpit"),
            ("/accounts/acct%2Fone/cockpit", "/cockpit"),
            ("/accounts/acc1/rules", "/rules"),
            ("/accounts/acct%2Fone/rules", "/rules"),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::PERMANENT_REDIRECT,
                "{uri} must permanently redirect"
            );
            assert_eq!(location_of(&response), expected, "{uri} target");
        }
    }

    /// The redirects live on the root router; the `/api` surface uses the same
    /// `/accounts/...` shapes and must keep serving JSON.
    #[tokio::test]
    async fn api_account_routes_are_not_redirected() {
        let (state, _, _) = test_state();
        let app = dashboard_router(state);

        for uri in ["/api/accounts/acc1/rules", "/api/accounts/acc1/cockpit"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_ne!(
                response.status(),
                StatusCode::PERMANENT_REDIRECT,
                "{uri} is an API route and must not redirect"
            );
            assert!(
                response.headers().get(header::LOCATION).is_none(),
                "{uri} must not carry a Location header"
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

    /// Redirecting (or emitting) a path the SvelteKit bundle cannot route just
    /// relocates the 404. Every canonical target must exist in the embedded
    /// client route table, and the generated message path must actually match the
    /// reader route id.
    #[test]
    fn embedded_spa_bundle_routes_every_canonical_deep_link_target() {
        let entry = embedded_spa_entry_chunk();

        for route_id in [CONTROL_ROUTE_ID, "/cockpit", "/rules"] {
            assert!(
                entry.contains(&format!("\"{route_id}\"")),
                "canonical route {route_id} missing from the embedded route table — deep \
                 links to it render the SvelteKit 404 page"
            );
        }

        let generated = crate::ui_paths::message_dashboard_path(
            "109c5747-8498-4614-945a-837462ae0aaf",
            "[Gmail]/Sent Mail",
            33281,
        );
        let path = generated.split('?').next().unwrap();
        assert!(
            route_id_matches(CONTROL_ROUTE_ID, path),
            "generated message link {generated} is not matched by client route {CONTROL_ROUTE_ID}"
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
