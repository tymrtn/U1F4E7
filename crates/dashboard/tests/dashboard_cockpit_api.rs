// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Integration tests for the v2 Agent Cockpit aggregate API surface
// (`/api/agents`, `/api/scheduled`, `/api/watches`). These build the real
// dashboard router over an in-memory DB with NO IMAP connection configured, so
// a 200 with populated data proves the aggregate load never probed a mailbox —
// the binding cockpit invariant that aggregate endpoints stay read-only.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database, DraftStatus};
use serde_json::Value;
use tower::ServiceExt;

fn seeded_state() -> AppState {
    let db = Database::open_memory().unwrap();
    db.conn().execute(
        "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password)
         VALUES ('acc1', 'Test', 'op@example.com', 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'x')",
        [],
    ).unwrap();

    // An agent with one attributed event.
    let agent = db.create_agent("skippy").unwrap();
    db.insert_event_with_agent(
        &envelope_email_store::Event {
            id: "e1".into(),
            account_id: "acc1".into(),
            event_type: "agent.action".into(),
            folder: "INBOX".into(),
            uid: None,
            message_id: None,
            from_addr: None,
            subject: None,
            snippet: None,
            payload: None,
            idempotency_key: None,
            secure_pending: false,
            acked_at: None,
            created_at: "2026-07-01T00:00:00".into(),
        },
        Some(&agent.identity.id),
    )
    .unwrap();

    // A pending-review draft awaiting approval.
    let pending = db
        .create_draft(
            "acc1",
            "approval-private@example.test",
            Some("Review me"),
            Some("approval-private-body"),
            None,
            None,
            None,
            None,
            Some("mcp"),
        )
        .unwrap();
    db.update_draft_status(&pending.id, DraftStatus::PendingReview)
        .unwrap();

    // A scheduled (due) draft with a blocking Governor verdict.
    let scheduled = db
        .create_draft(
            "acc1",
            "counterparty@gmail.com",
            Some("Queued"),
            Some("body"),
            None,
            None,
            None,
            None,
            Some("agent"),
        )
        .unwrap();
    db.update_draft_send_after(&scheduled.id, "2000-01-01T00:00:00")
        .unwrap();
    let gov_payload = serde_json::json!({
        "request": {"surface": "scheduled", "draft_id": scheduled.id},
        "outcome": {"allowed": false, "decision": "review", "block_code": "governor_blocked"},
    });
    db.insert_event(&envelope_email_store::Event {
        id: "g1".into(),
        account_id: "acc1".into(),
        event_type: "send_governor.blocked".into(),
        folder: "policy".into(),
        uid: None,
        message_id: None,
        from_addr: None,
        subject: None,
        snippet: None,
        payload: Some(gov_payload.to_string()),
        idempotency_key: None,
        secure_pending: false,
        acked_at: Some("2026-07-02T00:00:00".into()),
        created_at: "2026-07-02T00:00:00".into(),
    })
    .unwrap();

    // A watch + a route with a dead-lettered delivery.
    db.upsert_watch(envelope_email_store::WatchUpsert {
        account_id: "acc1",
        folder: "INBOX",
        status: "running",
        process_id: Some(1),
        schedule: Some("foreground"),
        last_heartbeat_at: Some("2026-07-08T08:59:00"),
        last_event_at: Some("2026-07-08T08:58:00"),
        failure_reason: None,
    })
    .unwrap();
    let route = db
        .create_event_route(
            "acc1",
            r#"{"event_types":["new_message"]}"#,
            r#"{"type":"webhook","url":"https://example.test/hook"}"#,
            true,
            100,
        )
        .unwrap();
    db.enqueue_delivery("d1", "e1", &route.id, "dk1", "2000-01-01T00:00:00")
        .unwrap();
    db.record_delivery_failure(
        "d1",
        Some(500),
        None,
        Some("boom"),
        None,
        "2026-07-08T00:00:00",
    )
    .unwrap();

    AppState::new(db, CredentialBackend::File)
}

async fn get_json(state: AppState, uri: &str) -> (StatusCode, Value) {
    let app = dashboard_router(state);
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn agents_endpoint_returns_roster_and_approval_queue_without_imap() {
    let (status, json) = get_json(seeded_state(), "/api/agents").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "aggregate agents load must not need IMAP"
    );
    assert_eq!(json["summary"]["agents"], 1);
    assert_eq!(json["agents"][0]["name"], "skippy");
    assert!(
        json["agents"][0]["token_prefix"]
            .as_str()
            .unwrap()
            .starts_with("envtok_")
    );
    assert_eq!(json["agents"][0]["activity"]["event_count"], 1);
    assert_eq!(json["summary"]["awaiting_approval"], 1);
    assert_eq!(json["approval_queue"][0]["source"], "mcp");
    // Aggregate queues expose workflow metadata only, never secrets or email content.
    let serialized = serde_json::to_string(&json).unwrap();
    assert!(!serialized.contains("token_hash"));
    assert!(!serialized.contains("approval-private@example.test"));
    assert!(!serialized.contains("approval-private-body"));
}

#[tokio::test]
async fn scheduled_endpoint_surfaces_governor_verdict_read_only() {
    let (status, json) = get_json(seeded_state(), "/api/scheduled").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["summary"]["scheduled"], 1);
    assert_eq!(json["summary"]["due"], 1);
    assert_eq!(json["scheduled"][0]["due"], true);
    assert_eq!(json["scheduled"][0]["governor"]["decision"], "review");
    assert_eq!(json["scheduled"][0]["governor"]["verdict"], "review");
    // Recipient address must never cross the aggregate surface.
    assert!(
        !serde_json::to_string(&json)
            .unwrap()
            .contains("counterparty@gmail.com")
    );
}

#[tokio::test]
async fn scheduled_scoped_endpoint_resolves_account_by_username() {
    let (status, json) = get_json(seeded_state(), "/api/accounts/op@example.com/scheduled").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["account_status"], "selected");
    assert_eq!(json["summary"]["scheduled"], 1);
}

#[tokio::test]
async fn watches_endpoint_exposes_secret_prefix_and_dead_letter_only() {
    let (status, json) = get_json(seeded_state(), "/api/watches").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["summary"]["watches"], 1);
    assert_eq!(json["summary"]["dead_letter"], 1);
    assert_eq!(json["watches"][0]["health"], "ok");
    assert_eq!(json["routes"][0]["health"], "danger");
    let prefix = json["routes"][0]["secret_prefix"].as_str().unwrap();
    assert_eq!(prefix.len(), 10);
    assert!(prefix.starts_with("evrt_"));
}
