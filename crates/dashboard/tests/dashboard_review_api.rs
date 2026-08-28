// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Integration tests for `GET /api/review` — the operator review queue. Built on
// the real dashboard router over an in-memory DB with NO IMAP connection
// configured, so a 200 with populated data proves the aggregate load never
// probed a mailbox. The seed deliberately selects no account anywhere: the
// review queue must aggregate drafts, rules, and events globally rather than
// silently returning empty groups the way the generic cockpit endpoint did.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database, DraftStatus};
use serde_json::Value;
use tower::ServiceExt;

fn seeded_state() -> (AppState, String) {
    let db = Database::open_memory().unwrap();
    db.conn().execute(
        "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password)
         VALUES ('acc1', 'Work', 'op@example.com', 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'x')",
        [],
    ).unwrap();

    // A pending-review draft — must surface globally, without ?account_id.
    let pending = db
        .create_draft(
            "acc1",
            "review-private@example.test",
            Some("Approve outreach"),
            Some("review-private-body"),
            None,
            None,
            None,
            None,
            Some("mcp"),
        )
        .unwrap();
    db.update_draft_status(&pending.id, DraftStatus::PendingReview)
        .unwrap();

    // A due scheduled send.
    let scheduled = db
        .create_draft(
            "acc1",
            "counterparty@example.test",
            Some("Queued"),
            Some("queued-private-body"),
            None,
            None,
            None,
            None,
            Some("agent"),
        )
        .unwrap();
    db.update_draft_send_after(&scheduled.id, "2000-01-01T00:00:00")
        .unwrap();

    // A due snooze.
    db.create_snoozed(
        "acc1",
        42,
        "INBOX",
        "Snoozed",
        "2000-01-01T00:00:00",
        None,
        Some("Due follow-up"),
        Some("review"),
        None,
        None,
    )
    .unwrap();

    // A proposed (disabled) rule.
    db.create_rule_with_enabled(
        "acc1",
        "Proposed junk sweep",
        r#"{"subject_contains":"newsletter"}"#,
        r#"{"move":"Junk"}"#,
        10,
        false,
        false,
    )
    .unwrap();

    // A message-anchored unacked event for triage.
    db.insert_event(&envelope_email_store::Event {
        id: "evt-1".into(),
        account_id: "acc1".into(),
        event_type: "watch.message_matched".into(),
        folder: "INBOX".into(),
        uid: Some(101),
        message_id: Some("<msg-101@example.com>".into()),
        from_addr: Some("sender@example.com".into()),
        subject: Some("Invoice due".into()),
        snippet: Some("Please pay".into()),
        payload: None,
        idempotency_key: None,
        secure_pending: false,
        acked_at: None,
        created_at: "2026-07-01T00:00:00".into(),
    })
    .unwrap();

    // A failed auth attempt for operational health.
    db.record_failed_auth(
        "acc1",
        "imap",
        "LOGIN failed for password=secret-token",
        Some("Create an app password and retry verification."),
    )
    .unwrap();

    (AppState::new(db, CredentialBackend::File), pending.id)
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
async fn review_endpoint_aggregates_globally_without_account_or_imap() {
    let (state, pending_id) = seeded_state();
    let (status, json) = get_json(state, "/api/review").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "aggregate review load must not need IMAP"
    );

    // Drafts surface with NO account selected — the cockpit regression.
    assert_eq!(json["decide_now"]["drafts"]["counts"]["pending_review"], 1);
    assert_eq!(
        json["decide_now"]["drafts"]["items"][0]["link"],
        format!("/accounts/acc1/drafts/{pending_id}")
    );
    assert_eq!(json["decide_now"]["proposed_rules"]["count"], 1);
    assert_eq!(
        json["decide_now"]["proposed_rules"]["items"][0]["review_state"],
        "proposed_disabled"
    );
    assert_eq!(json["waiting"]["scheduled"]["due"], 1);
    assert_eq!(json["waiting"]["due_snoozes"]["count"], 1);
    assert_eq!(json["needs_triage"]["count"], 1);
    assert_eq!(
        json["needs_triage"]["items"][0]["message_link"],
        "/mail/unified/acc1/101?folder=INBOX"
    );
    assert_eq!(json["operational_health"]["failed_auth"]["count"], 1);
    assert!(json["generated_at"].is_string());
    for group in [
        "decide_now",
        "waiting",
        "needs_triage",
        "operational_health",
    ] {
        assert!(
            json["summary"][group].is_u64(),
            "summary must carry a {group} count"
        );
    }

    // Safe summaries: no recipient addresses, draft bodies, or secrets.
    let serialized = serde_json::to_string(&json).unwrap();
    assert!(!serialized.contains("review-private@example.test"));
    assert!(!serialized.contains("review-private-body"));
    assert!(!serialized.contains("counterparty@example.test"));
    assert!(!serialized.contains("queued-private-body"));
    assert!(!serialized.contains("secret-token"));
}
