// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Integration tests for the cross-account Sent surface (`/api/messages/sent`).
// These build the real dashboard router over an in-memory DB with NO IMAP
// connection configured: a 200 with populated data proves the read path is
// served entirely from the local index (populated by the hourly sweep /
// refresh endpoint), never by probing a mailbox.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::models::IndexedMessageInput;
use envelope_email_store::{CredentialBackend, Database};
use serde_json::Value;
use tower::ServiceExt;

fn msg(uid: u32, date: &str, subject: &str, to_addr: &str) -> IndexedMessageInput {
    IndexedMessageInput {
        uid,
        message_id: Some(format!("<{uid}@x>")),
        from_addr: "me@example.test".into(),
        to_addr: to_addr.into(),
        subject: subject.into(),
        date: Some(date.into()),
        flags: vec!["\\Seen".into()],
        size: 1,
        snippet: None,
        thread_id: None,
    }
}

fn insert_account(db: &Database, id: &str, username: &str) {
    db.conn().execute(
        &format!(
            "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password)
             VALUES ('{id}', 'Test', '{username}', 'example.test', 'smtp.example.test', 587, 'imap.example.test', 993, 'x')"
        ),
        [],
    ).unwrap();
}

fn seeded_state() -> AppState {
    let db = Database::open_memory().unwrap();
    insert_account(&db, "acct-gmail", "g@example.test");
    insert_account(&db, "acct-dovecot", "d@example.test");
    db.set_detected_folder("acct-gmail", "sent", "[Gmail]/Sent Mail")
        .unwrap();
    db.set_detected_folder("acct-dovecot", "sent", "INBOX.Sent")
        .unwrap();
    db.upsert_indexed_message_summaries(
        "acct-gmail",
        "[Gmail]/Sent Mail",
        1,
        &[msg(
            7,
            "Sun, 23 Aug 2026 10:00:00 +0000",
            "gmail-sent",
            "alice@example.test",
        )],
    )
    .unwrap();
    db.upsert_indexed_message_summaries(
        "acct-dovecot",
        "INBOX.Sent",
        1,
        &[msg(
            9,
            "Sat, 22 Aug 2026 10:00:00 +0000",
            "dovecot-sent",
            "bob@example.test",
        )],
    )
    .unwrap();
    // An indexed INBOX on the same account must never leak into the Sent list.
    db.upsert_indexed_message_summaries(
        "acct-gmail",
        "INBOX",
        1,
        &[msg(
            1,
            "Mon, 24 Aug 2026 10:00:00 +0000",
            "inbox-noise",
            "me@example.test",
        )],
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
async fn sent_endpoint_merges_each_accounts_detected_folder_from_the_index() {
    let (status, json) = get_json(seeded_state(), "/api/messages/sent").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["scope"], "sent");

    let subjects: Vec<&str> = json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["subject"].as_str().unwrap())
        .collect();
    assert_eq!(
        subjects,
        vec!["gmail-sent", "dovecot-sent"],
        "newest first, one row per account's own Sent folder, no INBOX leak"
    );

    // Each row names its real per-account folder, so reader deep links and
    // bulk dispatch target the true mailbox.
    let folders: Vec<&str> = json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["folder"].as_str().unwrap())
        .collect();
    assert_eq!(folders, vec!["[Gmail]/Sent Mail", "INBOX.Sent"]);
}

#[tokio::test]
async fn sent_endpoint_reports_per_account_freshness() {
    let (status, json) = get_json(seeded_state(), "/api/messages/sent").await;
    assert_eq!(status, StatusCode::OK);
    let accounts = json["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);
    for account in accounts {
        assert_eq!(account["freshness"], "fresh");
        assert_eq!(account["message_count"], 1);
    }
}
