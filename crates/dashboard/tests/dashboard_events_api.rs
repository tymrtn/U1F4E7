// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//! `GET /api/events` — the Logs page's read. Newest first, same-day runs of
//! one event about one draft collapsed into a single entry, filterable by
//! account / type / since, paged with a `before` cursor. Read-only, and it
//! projects a fixed field set: never the raw payload, the snippet, or the
//! draft body.
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database, Event};
use serde_json::{Value, json};
use tower::ServiceExt;

const ACCOUNT: &str = "acc-logs";
const OTHER: &str = "acc-other";
const BODY_SENTINEL: &str = "private-body-logs-sentinel";
const SNIPPET_SENTINEL: &str = "private-snippet-logs-sentinel";
const HASH_SENTINEL: &str = "sha256:private-hash-logs-sentinel";
const SUBJECT: &str = "Plus Ultra inbox placement check";

fn event(id: &str, account: &str, event_type: &str, created_at: &str, payload: Value) -> Event {
    Event {
        id: id.to_string(),
        account_id: account.to_string(),
        event_type: event_type.to_string(),
        folder: "policy".to_string(),
        uid: None,
        message_id: None,
        from_addr: None,
        subject: None,
        snippet: Some(SNIPPET_SENTINEL.to_string()),
        payload: Some(payload.to_string()),
        idempotency_key: None,
        secure_pending: false,
        acked_at: Some(created_at.to_string()),
        created_at: created_at.to_string(),
    }
}

fn blocked_payload(draft_id: &str) -> Value {
    json!({
        "outcome": {
            "allowed": false,
            "decision": "review",
            "block_code": "governor_blocked",
            "block_reason": "governor decision 'review' did not permit this send",
            "attribution": {
                "declared_attrs": ["informational"],
                "derived_attrs": ["cold_email", "short_body"],
                "governor_attrs": ["cold_email", "informational", "short_body"]
            }
        },
        "request": {
            "draft_id": draft_id,
            "surface": "scheduled",
            "subject_hash": HASH_SENTINEL
        }
    })
}

fn seeded_state() -> (AppState, String) {
    let db = Database::open_memory().unwrap();
    for (id, username, display) in [
        (ACCOUNT, "desk@example.test", "Member Desk"),
        (OTHER, "other@example.test", "Other"),
    ] {
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password, display_name)
                 VALUES (?1, ?2, ?2, 'example.test', 'smtp.example.test', 587,
                         'imap.example.test', 993, 'encrypted', ?3)",
                (id, username, display),
            )
            .unwrap();
    }
    let draft = db
        .create_draft(
            ACCOUNT,
            "someone@example.test",
            Some(SUBJECT),
            Some(BODY_SENTINEL),
            None,
            None,
            None,
            None,
            Some("agent"),
        )
        .unwrap();
    let d = draft.id.clone();

    // A same-day run: the sweep re-evaluated this draft three times.
    for (i, at) in [
        "2026-09-17T10:00:00Z",
        "2026-09-17T10:05:00Z",
        "2026-09-17T10:10:00Z",
    ]
    .iter()
    .enumerate()
    {
        db.insert_event(&event(
            &format!("blk-{i}"),
            ACCOUNT,
            "send_governor.blocked",
            at,
            blocked_payload(&d),
        ))
        .unwrap();
    }
    // The day before: its own entry, never merged across days.
    db.insert_event(&event(
        "blk-prev",
        ACCOUNT,
        "send_governor.blocked",
        "2026-09-16T23:00:00Z",
        blocked_payload(&d),
    ))
    .unwrap();
    db.insert_event(&event(
        "approved",
        ACCOUNT,
        "draft_approved",
        "2026-09-17T11:00:00Z",
        json!({ "draft_id": d }),
    ))
    .unwrap();
    db.insert_event_with_agent(
        &event(
            "human-send",
            ACCOUNT,
            "send.human_dashboard",
            "2026-09-17T09:00:00Z",
            json!({ "draft_id": d, "surface": "human:dashboard", "governor": "skipped" }),
        ),
        Some("agent-x"),
    )
    .unwrap();
    let mut incoming = event(
        "msg",
        OTHER,
        "new_message",
        "2026-09-17T12:00:00Z",
        json!({}),
    );
    incoming.folder = "INBOX".to_string();
    incoming.uid = Some(7);
    incoming.message_id = Some("<hi@example.test>".to_string());
    incoming.from_addr = Some("alice@example.test".to_string());
    incoming.subject = Some("Hi".to_string());
    incoming.payload = None;
    incoming.acked_at = None;
    db.insert_event(&incoming).unwrap();

    (AppState::new(db, CredentialBackend::File), d)
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value, String) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let raw = String::from_utf8(bytes.to_vec()).unwrap();
    let value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    (status, value, raw)
}

fn types(body: &Value) -> Vec<String> {
    body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_type"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn lists_newest_first_collapsing_same_day_repeats_and_projects_no_content() {
    let (state, draft_id) = seeded_state();
    let app = dashboard_router(state);
    let (status, body, raw) = get_json(&app, "/api/events").await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        types(&body),
        [
            "new_message",
            "draft_approved",
            "send_governor.blocked",
            "send.human_dashboard",
            "send_governor.blocked",
        ]
    );
    assert_eq!(
        body["next_before"],
        Value::Null,
        "five entries fit in one page"
    );

    let blocked = &body["entries"][2];
    assert_eq!(blocked["repeat_count"], 3);
    assert_eq!(blocked["first_at"], "2026-09-17T10:00:00Z");
    assert_eq!(blocked["created_at"], "2026-09-17T10:10:00Z");
    assert_eq!(blocked["account_label"], "Member Desk");
    assert_eq!(blocked["draft_id"], draft_id);
    assert_eq!(
        blocked["draft_link"],
        format!("/accounts/{ACCOUNT}/drafts/{draft_id}")
    );
    assert_eq!(blocked["draft_subject"], SUBJECT);
    assert_eq!(blocked["surface"], "scheduled");
    assert_eq!(blocked["decision"], "review");
    assert_eq!(blocked["block_code"], "governor_blocked");
    assert_eq!(
        blocked["block_reason"],
        "governor decision 'review' did not permit this send"
    );
    assert_eq!(
        blocked["attrs"],
        json!(["cold_email", "informational", "short_body"])
    );
    assert_eq!(blocked["acked"], true);
    assert!(
        blocked["group_key"]
            .as_str()
            .unwrap()
            .contains("2026-09-17"),
        "group key carries the day bucket"
    );

    let previous_day = &body["entries"][4];
    assert_eq!(previous_day["repeat_count"], 1);
    assert_ne!(previous_day["group_key"], blocked["group_key"]);

    let human = &body["entries"][3];
    assert_eq!(human["agent_id"], "agent-x");
    assert_eq!(human["surface"], "human:dashboard");

    let message = &body["entries"][0];
    assert_eq!(message["account_label"], "Other");
    assert_eq!(message["from_addr"], "alice@example.test");
    assert_eq!(message["subject"], "Hi");
    assert_eq!(message["acked"], false);
    assert!(
        message["message_link"]
            .as_str()
            .unwrap()
            .starts_with("/mail/"),
        "message events deep-link to the reader"
    );
    assert_eq!(message["draft_link"], Value::Null);

    for private in [
        BODY_SENTINEL,
        SNIPPET_SENTINEL,
        HASH_SENTINEL,
        "\"payload\"",
        "\"snippet\"",
    ] {
        assert!(!raw.contains(private), "projection leaked {private}");
    }
}

#[tokio::test]
async fn filters_by_account_type_prefix_and_since() {
    let (state, _) = seeded_state();
    let app = dashboard_router(state);

    let (status, body, _) = get_json(&app, &format!("/api/events?account={OTHER}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(types(&body), ["new_message"]);

    let (_, body, _) = get_json(&app, "/api/events?type=send_governor").await;
    assert_eq!(
        types(&body),
        ["send_governor.blocked", "send_governor.blocked"]
    );

    let (_, body, _) = get_json(&app, "/api/events?type=draft_approved").await;
    assert_eq!(types(&body), ["draft_approved"]);

    let (_, body, _) = get_json(&app, "/api/events?since=2026-09-17T10:30:00Z").await;
    assert_eq!(types(&body), ["new_message", "draft_approved"]);

    // A dotted prefix is a namespace, not a substring: `send` matches
    // `send.human_dashboard` and never `send_governor.*`.
    let (_, body, _) = get_json(&app, "/api/events?type=send").await;
    assert_eq!(types(&body), ["send.human_dashboard"]);

    // Filters compose; `since` drops the previous day's run.
    let (_, body, _) = get_json(
        &app,
        &format!("/api/events?account={ACCOUNT}&type=send_governor&since=2026-09-17T00:00:00Z"),
    )
    .await;
    assert_eq!(types(&body), ["send_governor.blocked"]);
    assert_eq!(body["entries"][0]["repeat_count"], 3);
}

#[tokio::test]
async fn pages_with_the_before_cursor_and_ends_honestly() {
    let (state, _) = seeded_state();
    let app = dashboard_router(state);

    let (_, page1, _) = get_json(&app, "/api/events?limit=2").await;
    assert_eq!(types(&page1), ["new_message", "draft_approved"]);
    assert_eq!(page1["limit"], 2);
    assert_eq!(page1["next_before"], "2026-09-17T11:00:00Z");

    let (_, page2, _) = get_json(&app, "/api/events?limit=2&before=2026-09-17T11:00:00Z").await;
    assert_eq!(
        types(&page2),
        ["send_governor.blocked", "send.human_dashboard"]
    );
    assert_eq!(page2["next_before"], "2026-09-17T09:00:00Z");

    let (_, page3, _) = get_json(&app, "/api/events?limit=2&before=2026-09-17T09:00:00Z").await;
    assert_eq!(types(&page3), ["send_governor.blocked"]);
    assert_eq!(
        page3["next_before"],
        Value::Null,
        "a short page is the last page"
    );
}

#[tokio::test]
async fn rejects_malformed_queries_instead_of_guessing() {
    let (state, _) = seeded_state();
    let app = dashboard_router(state);
    for uri in [
        "/api/events?limit=0",
        "/api/events?limit=501",
        "/api/events?since=yesterday",
        "/api/events?before=2026-13-45",
        "/api/events?type=drop%20table",
    ] {
        let (status, body, _) = get_json(&app, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(body["code"], "events_query_invalid", "{uri}");
    }
}
