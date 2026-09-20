// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2

use axum::body::Body;
use axum::http::{Request, StatusCode};
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database, NewMailEngineDecision};
use serde_json::Value;
use tower::ServiceExt;

fn seeded_state() -> AppState {
    let db = Database::open_memory().unwrap();
    db.conn()
        .execute(
            "INSERT INTO accounts
             (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password)
             VALUES ('acc1', 'Test', 'op@example.test', 'example.test',
                     'smtp.example.test', 587, 'imap.example.test', 993, 'not-a-real-secret')",
            [],
        )
        .unwrap();
    db.plan_mail_engine_scan("acc1", "INBOX", 10, 40).unwrap();
    db.insert_mail_engine_decision_if_absent(&NewMailEngineDecision {
        account_id: "acc1",
        folder: "INBOX",
        uidvalidity: 10,
        uid: 41,
        input_hash: "private-input-hash",
        model: "typesafe/jev-1.13",
        status: "decided",
        route: "follow_up",
        route_probability: Some(0.95),
        route_confidence: Some(0.94),
        urgency: "urgent",
        notify_user_probability: Some(0.93),
        requires_reply_probability: Some(0.98),
        bulk_or_subscription_probability: Some(0.01),
        decision_json: r#"{"route":{"choice":"follow_up"}}"#,
    })
    .unwrap();
    db.conn()
        .execute(
            "INSERT INTO indexed_message_summaries
             (account_id, folder, uidvalidity, uid, from_addr, subject, date,
              flags_json, size, indexed_at)
             VALUES ('acc1', 'INBOX', 10, 41, 'sender@example.test',
                     'Please review', '2026-09-19T12:00:00Z', '[]', 0, datetime('now'))",
            [],
        )
        .unwrap();
    AppState::new(db, CredentialBackend::File)
}

async fn get_json(state: AppState, uri: &str) -> (StatusCode, Value) {
    let response = dashboard_router(state)
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn post_json(state: AppState, uri: &str, payload: Value) -> (StatusCode, Value) {
    let response = dashboard_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("cookie", "envelope_csrf=test-token")
                .header("x-envelope-csrf", "test-token")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn mail_engine_decisions_are_local_actionable_and_explicitly_untrusted() {
    let (status, json) = get_json(
        seeded_state(),
        "/api/mail-engine/decisions?route=follow_up&limit=20",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["state"], "available");
    assert_eq!(json["returned"], 1);
    assert_eq!(json["items"][0]["route"], "follow_up");
    assert_eq!(
        json["items"][0]["untrusted_content"]["subject"],
        "Please review"
    );
    assert_eq!(
        json["items"][0]["trust"]["instructions_authoritative"],
        false
    );
    assert!(
        json["items"][0]["message_link"]
            .as_str()
            .unwrap()
            .contains("/mail/unified/acc1/41")
    );
    let rendered = serde_json::to_string(&json).unwrap();
    assert!(!rendered.contains("private-input-hash"));
    assert!(!rendered.contains("decision_json"));
    assert!(!rendered.contains("not-a-real-secret"));
}

#[tokio::test]
async fn mail_engine_decision_limit_fails_closed() {
    let (status, json) = get_json(seeded_state(), "/api/mail-engine/decisions?limit=201").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json["code"], "invalid_mail_engine_limit");
}

#[tokio::test]
async fn correction_is_revision_guarded_and_preserves_the_model_result() {
    let state = seeded_state();
    let (status, corrected) = post_json(
        state.clone(),
        "/api/accounts/acc1/mail-engine/decisions/41/correction",
        serde_json::json!({
            "folder": "INBOX",
            "expected_revision": 0,
            "route": "important",
            "urgency": "critical"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(corrected["revision"], 1);

    let (status, decisions) = get_json(
        state.clone(),
        "/api/mail-engine/decisions?route=important&limit=20",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decisions["items"][0]["model_status"], "decided");
    assert_eq!(decisions["items"][0]["status"], "decided");
    assert_eq!(decisions["items"][0]["model_route"], "follow_up");
    assert_eq!(decisions["items"][0]["route"], "important");
    assert_eq!(decisions["items"][0]["correction_revision"], 1);

    let (status, conflict) = post_json(
        state,
        "/api/accounts/acc1/mail-engine/decisions/41/correction",
        serde_json::json!({
            "folder": "INBOX",
            "expected_revision": 0,
            "route": "routine",
            "urgency": "not_urgent"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "engine_revision_conflict");
}
