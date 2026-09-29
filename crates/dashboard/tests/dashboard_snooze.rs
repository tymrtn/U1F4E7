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

fn state_with_snoozes() -> AppState {
    let db = Database::open_memory().unwrap();
    db.conn()
        .execute(
            "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
             imap_host, imap_port, encrypted_password)
             VALUES ('acc1', 'A', 'a@example.test', 'example.test', 'smtp.example.test', 587,
                     'imap.example.test', 993, 'encrypted'),
                    ('acc2', 'B', 'b@example.test', 'example.test', 'smtp.example.test', 587,
                     'imap.example.test', 993, 'encrypted')",
            [],
        )
        .unwrap();
    // Owned by acc2, with a Message-ID.
    db.conn()
        .execute(
            "INSERT INTO snoozed (id, account, uid, original_folder, snoozed_folder, return_at,
             message_id, subject) VALUES ('other', 'acc2', 9, 'INBOX', 'Snoozed',
             '2026-11-01T13:00:00', 'm@x', 'Theirs')",
            [],
        )
        .unwrap();
    // Owned by acc1, legacy row without a Message-ID.
    db.conn()
        .execute(
            "INSERT INTO snoozed (id, account, uid, original_folder, snoozed_folder, return_at,
             message_id, subject) VALUES ('legacy', 'acc1', 7, 'INBOX', 'Snoozed',
             '2026-11-01T13:00:00', NULL, 'Mine')",
            [],
        )
        .unwrap();
    AppState::new(db, CredentialBackend::File)
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn unsnooze_refuses_a_record_owned_by_another_account_before_imap() {
    let app = dashboard_router(state_with_snoozes());
    let (status, _) = post_json(&app, "/api/accounts/acc1/snoozed/other/unsnooze", "{}").await;
    // 404, not 502: the ownership check ran before any socket was opened.
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unsnooze_without_message_id_refuses_instead_of_guessing_a_uid() {
    let app = dashboard_router(state_with_snoozes());
    let (status, body) = post_json(&app, "/api/accounts/acc1/snoozed/legacy/unsnooze", "{}").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "snooze_target_unknown");
}

#[tokio::test]
async fn snoozed_list_serves_utc_return_time_and_state() {
    let app = dashboard_router(state_with_snoozes());
    let (status, body) = get_json(&app, "/api/accounts/acc1/snoozed").await;
    assert_eq!(status, StatusCode::OK);
    let items = body["snoozed"].as_array().unwrap();
    assert_eq!(items.len(), 1, "only this account's snoozes: {body}");
    assert_eq!(items[0]["id"], "legacy");
    assert_eq!(items[0]["account_id"], "acc1");
    assert_eq!(items[0]["return_at"], "2026-11-01T13:00:00Z");
    assert!(matches!(
        items[0]["status"].as_str(),
        Some("snoozed" | "overdue")
    ));
}
