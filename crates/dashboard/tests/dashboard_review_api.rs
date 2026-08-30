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
        snippet: Some("event-snippet-private-body".into()),
        payload: Some(r#"{"raw":"event-payload-private-material"}"#.into()),
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

    // Observed thread history for the sent-history context group: two
    // outbound messages to one counterparty, subjects and snippets carrying
    // sentinels that must never cross the aggregate.
    let thread = db
        .create_thread(
            "itinerary",
            "2025-06-01T00:00:00",
            "2025-11-15T00:00:00",
            "acc1",
        )
        .unwrap();
    for (uid, date) in [(1u32, "2025-06-01T08:00:00"), (2, "2025-11-15T09:30:00")] {
        db.upsert_thread_message(
            &thread.thread_id,
            uid,
            Some(&format!("<sent-{uid}@fixture.test>")),
            None,
            None,
            "Sent",
            "op@example.com",
            "history-counterparty@x.test",
            None,
            None,
            date,
            "thread-subject-private-material",
            true,
            Some("thread-snippet-private-body"),
        )
        .unwrap();
    }

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
    // Capped sources declare how much of the queue the item list covers.
    assert_eq!(json["needs_triage"]["returned"], 1);
    assert_eq!(json["needs_triage"]["truncated"], false);
    assert_eq!(json["waiting"]["scheduled"]["returned"], 1);
    assert_eq!(json["waiting"]["scheduled"]["truncated"], false);
    assert_eq!(
        json["needs_triage"]["items"][0]["message_link"],
        "/mail/unified/acc1/101?folder=INBOX"
    );
    // Operator context survives redaction: subject and sender identify the
    // message without carrying its body.
    assert_eq!(json["needs_triage"]["items"][0]["subject"], "Invoice due");
    assert_eq!(
        json["needs_triage"]["items"][0]["from_addr"],
        "sender@example.com"
    );
    assert_eq!(json["operational_health"]["failed_auth"]["count"], 1);

    // Sent relationship history rides after the queue groups as context:
    // exact counterparty identity, observed counts, a truthful signal, and
    // an explicit no-link state instead of an invented destination.
    assert_eq!(json["sent_history"]["source"], "observed_thread_history");
    assert!(
        json["sent_history"]["coverage"]
            .as_str()
            .unwrap()
            .contains("not a complete mailbox census")
    );
    assert_eq!(json["sent_history"]["count"], 1);
    let history_item = &json["sent_history"]["items"][0];
    assert_eq!(history_item["counterparty"], "history-counterparty@x.test");
    assert_eq!(history_item["outbound_count"], 2);
    assert_eq!(history_item["inbound_count"], 0);
    assert_eq!(history_item["thread_count"], 1);
    assert_eq!(history_item["signal"], "historical_one_way");
    assert_eq!(history_item["link"], Value::Null);
    assert_eq!(history_item["link_state"], "not_available");
    // History is context: it never inflates the decision summary or Waiting.
    assert!(json["summary"].get("sent_history").is_none());
    assert_eq!(json["waiting"]["awaiting_reply"]["count"], 0);

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

    // Safe summaries: no recipient addresses, draft bodies, event snippets,
    // raw event payloads, or secrets.
    let serialized = serde_json::to_string(&json).unwrap();
    assert!(!serialized.contains("review-private@example.test"));
    assert!(!serialized.contains("review-private-body"));
    assert!(!serialized.contains("counterparty@example.test"));
    assert!(!serialized.contains("queued-private-body"));
    assert!(!serialized.contains("event-snippet-private-body"));
    assert!(!serialized.contains("event-payload-private-material"));
    assert!(!serialized.contains("thread-subject-private-material"));
    assert!(!serialized.contains("thread-snippet-private-body"));
    assert!(!serialized.contains("\"snippet\""));
    assert!(!serialized.contains("\"payload\""));
    assert!(!serialized.contains("secret-token"));
    // Durable free-text columns stay out of the aggregate entirely: no
    // auth reasons or guidance, snooze reasons/notes, action justifications,
    // watch failure prose, or route match expressions.
    assert!(!serialized.contains("Create an app password"));
    assert!(!serialized.contains("\"reason\""));
    assert!(!serialized.contains("retry_guidance"));
    assert!(!serialized.contains("\"justification\""));
    assert!(!serialized.contains("\"note\""));
    assert!(!serialized.contains("failure_reason"));
    assert!(!serialized.contains("match_expr"));
}
