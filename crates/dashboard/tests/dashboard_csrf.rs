// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Integration tests for CSRF protection (security HIGH-2) on mutating `/api`
// routes. These build the real dashboard router via `dashboard_router` and
// exercise the auth + CSRF middleware stack end to end.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use envelope_email_dashboard::auth::AuthConfig;
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database};
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

/// A mutating route that exists on the protected router. We send an empty body,
/// so once past the CSRF layer the `Json` extractor rejects it (4xx) before the
/// handler does any network work. We only assert whether the *CSRF layer*
/// rejected the request (403 `dashboard_csrf_required`) or let it through.
const MUTATING_URI: &str = "/api/accounts/discover";

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
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

fn is_csrf_403(status: StatusCode, body: &[u8]) -> bool {
    if status != StatusCode::FORBIDDEN {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(body)
        .map(|j| j["code"] == "dashboard_csrf_required")
        .unwrap_or(false)
}

#[tokio::test]
async fn identity_auth_mutating_post_without_csrf_is_rejected() {
    let app = dashboard_router(state().with_auth(AuthConfig::from_parts(
        None,
        ["skippy@tail.ts.net".to_string()],
    )));
    let (status, body) = send(
        &app,
        "POST",
        MUTATING_URI,
        &[("tailscale-user-login", "skippy@tail.ts.net")],
    )
    .await;
    assert!(
        is_csrf_403(status, &body),
        "identity-authorized POST with no CSRF token must be 403 dashboard_csrf_required, got {status}"
    );
}

#[tokio::test]
async fn open_mode_mutating_post_without_csrf_is_rejected() {
    // Cookie-less mutating browser request in open loopback mode still enforces
    // CSRF — this is the forgery case a malicious page would drive.
    let app = dashboard_router(state());
    let (status, body) = send(&app, "POST", MUTATING_URI, &[]).await;
    assert!(
        is_csrf_403(status, &body),
        "open-mode cookie-less POST must be 403 dashboard_csrf_required, got {status}"
    );
}

#[tokio::test]
async fn matching_cookie_and_header_passes_the_csrf_layer() {
    let app = dashboard_router(state());
    let (status, body) = send(
        &app,
        "POST",
        MUTATING_URI,
        &[
            ("cookie", "envelope_csrf=tok123"),
            ("x-envelope-csrf", "tok123"),
        ],
    )
    .await;
    assert!(
        !is_csrf_403(status, &body),
        "matching cookie+header must pass the CSRF layer (handler outcome may differ), got 403 csrf"
    );
}

#[tokio::test]
async fn bearer_token_post_without_csrf_is_exempt() {
    let app = dashboard_router(state().with_auth(AuthConfig::from_parts(Some("t0ken".into()), [])));
    let (status, body) = send(
        &app,
        "POST",
        MUTATING_URI,
        &[("authorization", "Bearer t0ken")],
    )
    .await;
    assert!(
        !is_csrf_403(status, &body),
        "bearer-authenticated POST must be CSRF-exempt, got 403 csrf"
    );
}

#[tokio::test]
async fn mismatched_cookie_and_header_is_rejected() {
    let app = dashboard_router(state());
    let (status, body) = send(
        &app,
        "POST",
        MUTATING_URI,
        &[
            ("cookie", "envelope_csrf=tok123"),
            ("x-envelope-csrf", "different"),
        ],
    )
    .await;
    assert!(
        is_csrf_403(status, &body),
        "mismatched cookie/header must be 403 csrf, got {status}"
    );
}

#[tokio::test]
async fn cross_origin_request_is_rejected() {
    let app = dashboard_router(state());
    let (status, body) = send(
        &app,
        "POST",
        MUTATING_URI,
        &[
            ("cookie", "envelope_csrf=tok123"),
            ("x-envelope-csrf", "tok123"),
            ("origin", "https://evil.example"),
            ("host", "localhost:3141"),
        ],
    )
    .await;
    assert!(
        is_csrf_403(status, &body),
        "cross-origin Origin header must be 403 csrf even with matching token, got {status}"
    );
}

#[tokio::test]
async fn cross_site_fetch_metadata_is_rejected() {
    let app = dashboard_router(state());
    let (status, body) = send(
        &app,
        "POST",
        MUTATING_URI,
        &[
            ("cookie", "envelope_csrf=tok123"),
            ("x-envelope-csrf", "tok123"),
            ("sec-fetch-site", "cross-site"),
        ],
    )
    .await;
    assert!(
        is_csrf_403(status, &body),
        "cross-site Sec-Fetch-Site must be 403 csrf, got {status}"
    );
}

#[tokio::test]
async fn get_never_requires_csrf() {
    // GET on a protected route in open mode: no cookie, no header, must not be
    // CSRF-rejected.
    let app = dashboard_router(state());
    let (status, body) = send(&app, "GET", "/api/accounts", &[]).await;
    assert!(
        !is_csrf_403(status, &body),
        "GET must never be CSRF-checked"
    );
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn csrf_endpoint_mints_token_and_sets_cookie() {
    let app = dashboard_router(state());
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/csrf")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let set_cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    // Plain-HTTP (no X-Forwarded-Proto): plain cookie name, no Secure, no __Host-.
    assert!(
        set_cookie.starts_with("envelope_csrf="),
        "loopback cookie must use plain name, got {set_cookie}"
    );
    assert!(set_cookie.contains("SameSite=Strict"));
    assert!(
        !set_cookie.contains("Secure"),
        "plain HTTP must not set Secure"
    );
    assert!(
        !set_cookie.contains("__Host-"),
        "no __Host- prefix without Secure"
    );

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let token = json["token"].as_str().unwrap();
    assert_eq!(token.len(), 64, "token is 32 bytes hex-encoded");
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

    // The minted cookie value must equal the returned token.
    let cookie_val = set_cookie
        .strip_prefix("envelope_csrf=")
        .and_then(|r| r.split(';').next())
        .unwrap();
    assert_eq!(cookie_val, token);
}

#[tokio::test]
async fn csrf_endpoint_uses_host_cookie_over_https() {
    let app = dashboard_router(state());
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/csrf")
                .header("x-forwarded-proto", "https")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    assert!(
        set_cookie.starts_with("__Host-envelope_csrf="),
        "HTTPS must use __Host- prefixed cookie, got {set_cookie}"
    );
    assert!(
        set_cookie.contains("Secure"),
        "__Host- prefix requires Secure"
    );
    assert!(set_cookie.contains("SameSite=Strict"));
}
