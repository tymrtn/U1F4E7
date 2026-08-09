// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Integration tests for the snooze endpoint (POST
// /api/accounts/{id}/messages/{uid}/snooze). These build the real router and
// assert that an invalid `return_at` is rejected with a stable JSON error
// BEFORE any IMAP work — i.e. bad input never touches the mailbox. Valid input
// would reach `get_or_create_imap` (a real socket), so we deliberately only
// exercise the pre-IMAP validation branch here, matching the crate's other
// handler tests.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
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

/// POST JSON past the CSRF layer (open mode + matching cookie/header token).
async fn post_json(app: &Router, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("cookie", "envelope_csrf=tok123")
                .header("x-envelope-csrf", "tok123")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn snooze_rejects_past_return_at_before_touching_imap() {
    let app = dashboard_router(state());
    let (status, body) = post_json(
        &app,
        "/api/accounts/acc1/messages/42/snooze",
        r#"{"folder":"INBOX","return_at":"2020-01-01T09:00:00Z"}"#,
    )
    .await;
    // 400 (not 502): validation ran and short-circuited before any IMAP work.
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "past time must be 400, got {status}"
    );
    assert_eq!(body["code"], "invalid_return_at");
}

#[tokio::test]
async fn snooze_rejects_empty_return_at_before_touching_imap() {
    let app = dashboard_router(state());
    let (status, body) = post_json(
        &app,
        "/api/accounts/acc1/messages/42/snooze",
        r#"{"folder":"INBOX","return_at":"   "}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "empty time must be 400, got {status}"
    );
    assert_eq!(body["code"], "invalid_return_at");
}
