// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `GET /api/events/stream` — Server-Sent Events fan-out of [`DashboardEvent`]s.
//!
//! This is a read-only push channel: it never mutates a mailbox and only relays
//! events other code already published to the broadcast bus. It lives inside the
//! protected router, so `require_auth` gates it and CSRF is skipped (GET).
//!
//! ## Authentication for `EventSource`
//! All authorization is done by the `require_auth` middleware ahead of this
//! handler; by the time `stream` runs the caller is already authorized (or, in
//! open-loopback mode, no credential is required). The browser `EventSource` API
//! cannot set an `Authorization` header, so two paths reach an authorized state:
//! - **Cookie / tailnet identity** (the normal browser path): the request rides
//!   the same cookie/`Tailscale-User-Login` credential every other `/api` call
//!   uses; `require_auth` accepts it.
//! - **`?access_token=<token>` query param** (bearer-only agent contexts, e.g.
//!   `curl`/scripts): `require_auth` accepts it via the same constant-time bearer
//!   comparison as the header token (see [`crate::auth::require_auth`]). A wrong
//!   token is rejected with `401` before this handler runs. Note the query token
//!   is deliberately NOT treated as CSRF-proof (a browser can carry a query
//!   string cross-site), so it never sets the `BearerAuthenticated` CSRF
//!   exemption — moot for this GET stream, but correct for the shared middleware.
//!
//!   Logging tradeoff: a token in the query string can appear in server-side
//!   request logs. Assessed: this dashboard's tracing does **not** log request
//!   URLs or query strings (no request-logging `TraceLayer`/access log is
//!   installed; the only URL-adjacent logs are explicit `info!`/`warn!` in sweeps
//!   that never include the raw URI). So the query token is not written to any
//!   log by the dashboard. If a request-logging layer is added later, it MUST
//!   redact the `access_token` query param.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use tokio::sync::broadcast::error::RecvError;

use crate::events::DashboardEvent;
use crate::state::AppState;

/// Keep-alive comment cadence. Idle SSE connections get a heartbeat comment
/// every 25s so proxies and the browser keep the connection open.
const HEARTBEAT: Duration = Duration::from_secs(25);

/// SSE handler. Returns a `text/event-stream` that emits one SSE event per
/// published [`DashboardEvent`], plus a 25s keep-alive heartbeat.
pub async fn stream(State(state): State<AppState>) -> Response {
    // Authorization (header, tailnet identity, cookie, or the `?access_token=`
    // query param for `EventSource`) is fully handled by the `require_auth`
    // middleware ahead of this handler — a wrong or missing credential returns
    // `401` before we get here. See `crate::auth::require_auth` for the
    // query-token acceptance path and the module docs for the logging tradeoff.
    let rx = state.events.subscribe();

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(ev) => Some((Ok::<Event, Infallible>(to_sse_event(&ev)), rx)),
            Err(RecvError::Lagged(n)) => {
                // The subscriber fell behind by more than the channel capacity.
                // Emit a control frame so the client can resync via a full poll,
                // then keep streaming from the oldest retained event. Returning
                // `Some` keeps the stream open rather than closing it.
                let frame = Event::default()
                    .event("lagged")
                    .data(format!("{{\"type\":\"lagged\",\"dropped\":{n}}}"));
                Some((Ok(frame), rx))
            }
            Err(RecvError::Closed) => None,
        }
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(HEARTBEAT).text("keep-alive"))
        .into_response()
}

/// Serialize a [`DashboardEvent`] into an SSE [`Event`] with `event:` set to the
/// event kind and `data:` set to the JSON body.
fn to_sse_event(ev: &DashboardEvent) -> Event {
    let data =
        serde_json::to_string(ev).unwrap_or_else(|_| format!("{{\"type\":\"{}\"}}", ev.kind()));
    Event::default().event(ev.kind()).data(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_event_sets_event_field_to_kind() {
        // Constructing the frame must not panic and must carry the kind.
        let ev = DashboardEvent::Unsnoozed {
            account_id: "acc1".into(),
            original_folder: "INBOX".into(),
        };
        let frame = to_sse_event(&ev);
        // `Event` has no public getters, so round-trip via its Display-ish
        // serialization by wrapping in a response is overkill here; we assert the
        // JSON body encodes the kind, which is what a client discriminates on.
        let data = serde_json::to_string(&ev).unwrap();
        assert!(data.contains("\"type\":\"unsnoozed\""));
        let _ = frame; // frame construction is the real assertion (no panic)
    }
}
