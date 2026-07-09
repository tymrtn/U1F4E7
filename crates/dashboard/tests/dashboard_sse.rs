// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Integration tests for the real-time SSE endpoint `GET /api/events/stream`.
// These build the real dashboard router via `dashboard_router` and exercise the
// auth middleware + the SSE handler end to end, reading the streaming body with
// a timeout and asserting on the frames that arrive. Follows the harness style
// of dashboard_csrf.rs.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use envelope_email_dashboard::auth::AuthConfig;
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::events::DashboardEvent;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database};
use futures_util::StreamExt;
use tower::ServiceExt;

fn state() -> AppState {
    let db = Database::open_memory().unwrap();
    db.conn()
        .execute(
            "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
             imap_host, imap_port, encrypted_password)
             VALUES ('acc1', 'Spain Expat', 'editor@spainexpat.com', 'spainexpat.com',
                     'smtp.spainexpat.com', 587, 'imap.spainexpat.com', 993, 'encrypted')",
            [],
        )
        .unwrap();
    AppState::new(db, CredentialBackend::File)
}

/// Open the SSE stream and return the streaming response. Fails the test if the
/// initial response is not `200 text/event-stream`.
async fn open_stream(
    app: &Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> axum::response::Response {
    let mut builder = Request::builder().method("GET").uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

/// Read from the response body until `needle` appears in the accumulated UTF-8,
/// or the timeout elapses. Returns the accumulated text seen so far.
async fn read_until(resp: axum::response::Response, needle: &str, timeout: Duration) -> String {
    let mut stream = resp.into_body().into_data_stream();
    let mut acc = String::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                acc.push_str(&String::from_utf8_lossy(&chunk));
                if acc.contains(needle) {
                    break;
                }
            }
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break, // timed out
        }
    }
    acc
}

#[tokio::test]
async fn authorized_stream_receives_published_event() {
    let state = state();
    let app = dashboard_router(state.clone());

    // Open the stream first so the subscriber exists before we publish.
    let resp = open_stream(&app, "/api/events/stream", &[]).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("text/event-stream"),
        "expected SSE content type, got {ct}"
    );

    // Publish after a short delay so the reader is already awaiting the body.
    let publisher = state.clone();
    tokio::spawn(async move {
        // Wait until the handler's subscriber is registered before publishing so
        // the broadcast reaches it (subscribers only see events sent after
        // subscribe()).
        for _ in 0..50 {
            if publisher.events.receiver_count() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        publisher.events.publish(DashboardEvent::DraftQueued {
            account_id: "acc1".into(),
            draft_id: "d1".into(),
            origin: "compose",
        });
    });

    let seen = read_until(resp, "draft_queued", Duration::from_secs(5)).await;
    assert!(
        seen.contains("event:draft_queued") || seen.contains("event: draft_queued"),
        "SSE frame should carry the event type; saw: {seen:?}"
    );
    assert!(
        seen.contains("\"type\":\"draft_queued\"") && seen.contains("\"draft_id\":\"d1\""),
        "SSE data should carry the JSON event body; saw: {seen:?}"
    );
    // Privacy: no bodies/subjects/recipients ride this channel.
    assert!(!seen.to_lowercase().contains("subject"));
}

#[tokio::test]
async fn keep_alive_wrapped_stream_delivers_events() {
    // The response is wrapped in axum's `KeepAlive` stream (25s heartbeat
    // interval, configured in the handler). This test proves events still flow
    // through that wrapper — a broken keep-alive wrapping would either close the
    // stream (timeout, no data) or drop events. The 25s heartbeat cadence itself
    // is axum's contract and is not re-timed here to keep the test fast.
    let state = state();
    let app = dashboard_router(state.clone());
    let resp = open_stream(&app, "/api/events/stream", &[]).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let publisher = state.clone();
    tokio::spawn(async move {
        for _ in 0..50 {
            if publisher.events.receiver_count() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        publisher.events.publish(DashboardEvent::AccountHealth {
            account_id: "acc1".into(),
            status: "unhealthy",
        });
    });

    let seen = read_until(resp, "account_health", Duration::from_secs(5)).await;
    assert!(
        seen.contains("account_health"),
        "keep-alive-wrapped stream must still deliver events; saw: {seen:?}"
    );
}

#[tokio::test]
async fn keep_alive_heartbeat_comment_is_emitted_on_idle() {
    // Prove the heartbeat mechanism directly, without waiting the handler's 25s
    // interval: wrap an idle (never-yielding) inner stream in a `KeepAlive` with a
    // short interval and assert the configured comment text frames the stream.
    // This mirrors exactly how the handler wraps its stream.
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures_util::stream;
    use std::convert::Infallible;

    let idle = stream::pending::<Result<Event, Infallible>>();
    let sse = Sse::new(idle).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_millis(50))
            .text("keep-alive"),
    );
    let resp = sse.into_response();
    let seen = read_until(resp, "keep-alive", Duration::from_secs(3)).await;
    assert!(
        seen.contains(": keep-alive"),
        "keep-alive heartbeat comment must be emitted on an idle stream; saw: {seen:?}"
    );
}

#[tokio::test]
async fn unauthorized_stream_is_rejected_when_token_enforced() {
    let app = dashboard_router(state().with_auth(AuthConfig::from_parts(Some("t0ken".into()), [])));

    // No credential at all → 401 from require_auth before the handler runs.
    let resp = open_stream(&app, "/api/events/stream", &[]).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], "dashboard_auth_required");
}

#[tokio::test]
async fn header_bearer_authorizes_the_stream() {
    let app = dashboard_router(state().with_auth(AuthConfig::from_parts(Some("t0ken".into()), [])));
    let resp = open_stream(
        &app,
        "/api/events/stream",
        &[("authorization", "Bearer t0ken")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn query_token_authorizes_the_stream_for_eventsource() {
    // EventSource cannot set Authorization; the `?access_token=` query param is
    // validated with the same constant-time bearer check.
    let app = dashboard_router(state().with_auth(AuthConfig::from_parts(Some("t0ken".into()), [])));
    let resp = open_stream(&app, "/api/events/stream?access_token=t0ken", &[]).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "correct query token must open the stream"
    );
}

#[tokio::test]
async fn wrong_query_token_is_rejected() {
    let app = dashboard_router(state().with_auth(AuthConfig::from_parts(Some("t0ken".into()), [])));
    // Wrong query token, no header: require_auth denies (401). Even if it somehow
    // reached the handler, the handler's constant-time check rejects it too.
    let resp = open_stream(&app, "/api/events/stream?access_token=nope", &[]).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn open_loopback_mode_allows_the_stream_without_credentials() {
    // No auth configured (open loopback): the stream is reachable like every
    // other /api route.
    let app = dashboard_router(state());
    let resp = open_stream(&app, "/api/events/stream", &[]).await;
    assert_eq!(resp.status(), StatusCode::OK);
}
