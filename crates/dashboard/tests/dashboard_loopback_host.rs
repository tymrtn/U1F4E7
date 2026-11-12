// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// With no credential configured, the dashboard answers only requests addressed
// to a loopback name. A browser page that reaches the port under any other
// name gets a 403 before any route runs.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use envelope_email_dashboard::auth::AuthConfig;
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database};
use tower::ServiceExt;

fn state() -> AppState {
    AppState::new(Database::open_memory().unwrap(), CredentialBackend::File)
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, serde_json::Value) {
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
        .unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn is_host_403(status: StatusCode, body: &serde_json::Value) -> bool {
    status == StatusCode::FORBIDDEN && body["code"] == "dashboard_host_not_loopback"
}

#[tokio::test]
async fn open_mode_refuses_a_foreign_host_on_every_surface() {
    let app = dashboard_router(state());
    for host in [
        "evil.example:3141",
        "evil.example",
        "localhost.evil.example:3141",
        "127.0.0.1.evil.example:3141",
        "rey.tail1234.ts.net",
    ] {
        for (method, uri) in [
            ("GET", "/"),
            ("GET", "/api/health"),
            ("GET", "/api/accounts"),
            ("POST", "/api/accounts/discover"),
        ] {
            let (status, body) = send(&app, method, uri, &[("host", host)]).await;
            assert!(
                is_host_403(status, &body),
                "{method} {uri} with Host {host} must be refused, got {status} {body}"
            );
        }
    }
}

#[tokio::test]
async fn refusal_says_how_to_reach_the_dashboard_without_echoing_the_host() {
    let app = dashboard_router(state());
    let (_, body) = send(
        &app,
        "GET",
        "/api/accounts",
        &[("host", "evil.example:3141")],
    )
    .await;
    let detail = body["detail"].as_str().unwrap();
    assert!(detail.contains("127.0.0.1"), "{detail}");
    assert!(detail.contains("dashboard.auth_token"), "{detail}");
    assert!(!body.to_string().contains("evil.example"), "{body}");
}

#[tokio::test]
async fn open_mode_answers_loopback_names() {
    let app = dashboard_router(state());
    for host in [
        "localhost:3141",
        "localhost",
        "LOCALHOST:3141",
        "127.0.0.1:52811",
        "127.0.0.1",
        "[::1]:3141",
        "[::1]",
    ] {
        let (status, body) = send(&app, "GET", "/api/accounts", &[("host", host)]).await;
        assert_eq!(status, StatusCode::OK, "Host {host}: {body}");
    }
}

#[tokio::test]
async fn open_mode_answers_requests_without_a_host_header() {
    // Browsers always send Host, so a request without one is a local
    // non-browser client.
    let app = dashboard_router(state());
    let (status, body) = send(&app, "GET", "/api/accounts", &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn token_mode_answers_a_tailnet_host_with_the_token() {
    let app = dashboard_router(state().with_auth(AuthConfig::from_parts(Some("t0ken".into()), [])));
    let (status, body) = send(
        &app,
        "GET",
        "/api/accounts",
        &[
            ("host", "rey.tail1234.ts.net"),
            ("authorization", "Bearer t0ken"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn an_allowed_login_on_any_other_host_is_unauthorized() {
    // `tailscale serve` forwards the tailnet name it was reached under, so a
    // login header under any other name came from somewhere else.
    for token in [None, Some("t0ken".to_string())] {
        let app = dashboard_router(state().with_auth(AuthConfig::from_parts(
            token,
            ["skippy@tail.ts.net".to_string()],
        )));
        for host in ["evil.example:3141", "127.0.0.1:3141", "localhost:3141"] {
            let (status, body) = send(
                &app,
                "GET",
                "/api/accounts",
                &[
                    ("host", host),
                    ("tailscale-user-login", "skippy@tail.ts.net"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "Host {host}: {body}");
        }
    }
}

#[tokio::test]
async fn identity_mode_answers_a_tailnet_host_for_an_allowed_login() {
    let app = dashboard_router(state().with_auth(AuthConfig::from_parts(
        None,
        ["skippy@tail.ts.net".to_string()],
    )));
    let (status, body) = send(
        &app,
        "GET",
        "/api/accounts",
        &[
            ("host", "rey.tail1234.ts.net"),
            ("tailscale-user-login", "skippy@tail.ts.net"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
