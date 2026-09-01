// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CONTEXT_REFINED_EVENT, CredentialBackend, Database, DraftStatus};
use envelope_email_transport::attribution_persist::PersistedDeclaration;
use serde_json::{Value, json};
use tower::ServiceExt;

const ACCOUNT: &str = "acc-context";
const OTHER_ACCOUNT: &str = "acc-other";
const RECIPIENT_SENTINEL: &str = "private-recipient-context@example.test";
const SUBJECT_SENTINEL: &str = "private-subject-context-sentinel";
const BODY_SENTINEL: &str = "private-body-context-sentinel";
const ATTACHMENT_SENTINEL: &str = "private-calendar-context-sentinel.ics";
const BYTES_SENTINEL: &str = "UFJJVkFURS1DQVRFREFSLUJZVEVT";

fn seeded_state() -> (AppState, String, i64) {
    let db = Database::open_memory().unwrap();
    for (id, username) in [
        (ACCOUNT, "owner@example.test"),
        (OTHER_ACCOUNT, "other@example.test"),
    ] {
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES (?1, 'Context account', ?2, 'example.test', 'smtp.example.test', 587,
                         'imap.example.test', 993, 'encrypted')",
                (id, username),
            )
            .unwrap();
    }

    let draft = db
        .create_draft(
            ACCOUNT,
            RECIPIENT_SENTINEL,
            Some(SUBJECT_SENTINEL),
            Some(BODY_SENTINEL),
            None,
            None,
            None,
            None,
            Some("agent"),
        )
        .unwrap();
    db.update_draft_attachments(
        &draft.id,
        &[json!({
            "filename": ATTACHMENT_SENTINEL,
            "content_type": "text/calendar; method=REQUEST",
            "size": 24,
            "data_base64": BYTES_SENTINEL,
        })],
    )
    .unwrap();
    db.set_draft_metadata(
        &draft.id,
        &json!({
            "send_block": {
                "code": "attributes_required",
                "title": "private-free-text-title-must-not-project",
                "explanation": "private-free-text-explanation-must-not-project",
                "action": "send"
            }
        }),
    )
    .unwrap();
    let current = db.get_draft(&draft.id).unwrap().unwrap();
    db.set_draft_attribution(
        &draft.id,
        &PersistedDeclaration::new_bot(&["informational".to_string()], current.revision).to_value(),
    )
    .unwrap();
    db.update_draft_status(&draft.id, DraftStatus::PendingReview)
        .unwrap();
    let parked = db.get_draft(&draft.id).unwrap().unwrap();
    (
        AppState::new(db, CredentialBackend::File),
        parked.id,
        parked.revision,
    )
}

async fn request_json(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    csrf: bool,
) -> (StatusCode, Value, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    if csrf {
        builder = builder
            .header("cookie", "envelope_csrf=context-token")
            .header("x-envelope-csrf", "context-token");
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(
                    body.map(|value| value.to_string()).unwrap_or_default(),
                ))
                .unwrap(),
        )
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

fn retry_body(revision: i64, attributes: &[&str]) -> Value {
    json!({
        "expected_revision": revision,
        "declarable_attributes": attributes,
        "confirm_factual_accuracy": true,
    })
}

#[tokio::test]
async fn projection_is_account_scoped_content_free_and_provenance_safe() {
    let (state, draft_id, revision) = seeded_state();
    let app = dashboard_router(state);
    let uri = format!("/api/accounts/{ACCOUNT}/drafts/{draft_id}/context-refinement");
    let (status, body, raw) = request_json(&app, Method::GET, &uri, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["eligible"], true);
    assert_eq!(body["revision"], revision);
    assert_eq!(body["action"], "send");
    assert_eq!(body["reason_code"], "attributes_required");

    for private in [
        RECIPIENT_SENTINEL,
        SUBJECT_SENTINEL,
        BODY_SENTINEL,
        ATTACHMENT_SENTINEL,
        BYTES_SENTINEL,
        "private-free-text-title-must-not-project",
        "private-free-text-explanation-must-not-project",
        "data_base64",
        "to_addr",
        "text_content",
        "html_content",
        "score",
        "weight",
        "threshold",
    ] {
        assert!(!raw.contains(private), "projection leaked {private}");
    }

    let attributes = body["attributes"].as_array().unwrap();
    let informational = attributes
        .iter()
        .find(|entry| entry["key"] == "informational")
        .unwrap();
    assert_eq!(informational["provenance"], "declarable");
    assert_eq!(informational["selectable"], true);
    let calendar = attributes
        .iter()
        .find(|entry| entry["key"] == "calendar_invitation")
        .unwrap();
    assert_eq!(calendar["provenance"], "host_derived");
    assert_eq!(calendar["state"], "observed");
    assert_eq!(calendar["read_only"], true);
    let approval = attributes
        .iter()
        .find(|entry| entry["key"] == "tyler_approved")
        .unwrap();
    assert_eq!(approval["provenance"], "requires_attestation");
    assert_eq!(approval["state"], "unavailable");
    assert_eq!(approval["selectable"], false);

    let wrong_uri = format!("/api/accounts/{OTHER_ACCOUNT}/drafts/{draft_id}/context-refinement");
    let (wrong_status, _, _) = request_json(&app, Method::GET, &wrong_uri, None, false).await;
    assert_eq!(wrong_status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn retry_requires_csrf_confirmation_and_declarable_only_input() {
    let (state, draft_id, revision) = seeded_state();
    let app = dashboard_router(state.clone());
    let uri = format!("/api/accounts/{ACCOUNT}/drafts/{draft_id}/context-refinement/retry");

    let (status, body, _) = request_json(
        &app,
        Method::POST,
        &uri,
        Some(retry_body(revision, &["informational"])),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "dashboard_csrf_required");

    let mut unconfirmed = retry_body(revision, &["informational"]);
    unconfirmed["confirm_factual_accuracy"] = Value::Bool(false);
    let (status, _, _) = request_json(&app, Method::POST, &uri, Some(unconfirmed), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    for forbidden in ["single_recipient", "tyler_approved", "invented_fact"] {
        let (status, body, raw) = request_json(
            &app,
            Method::POST,
            &uri,
            Some(retry_body(revision, &[forbidden])),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted {forbidden}");
        assert_eq!(body["code"], "context_refinement_invalid");
        assert!(!raw.contains(forbidden), "error echoed rejected key");
    }

    let draft = state.db.lock().await.get_draft(&draft_id).unwrap().unwrap();
    assert_eq!(draft.status, DraftStatus::PendingReview);
    assert!(draft.send_after.is_none());
    assert!(
        state
            .db
            .lock()
            .await
            .current_context_correction(ACCOUNT, &draft_id, revision)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn stale_revision_is_a_no_write_conflict() {
    let (state, draft_id, revision) = seeded_state();
    let app = dashboard_router(state.clone());
    let uri = format!("/api/accounts/{ACCOUNT}/drafts/{draft_id}/context-refinement/retry");
    let (status, _, _) = request_json(
        &app,
        Method::POST,
        &uri,
        Some(retry_body(revision + 1, &["informational"])),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let db = state.db.lock().await;
    let draft = db.get_draft(&draft_id).unwrap().unwrap();
    assert_eq!(draft.status, DraftStatus::PendingReview);
    assert!(draft.send_after.is_none());
    assert!(
        db.current_context_correction(ACCOUNT, &draft_id, revision)
            .unwrap()
            .is_none()
    );
    assert!(
        db.list_events(Some(ACCOUNT), 100)
            .unwrap()
            .iter()
            .all(|event| event.event_type != CONTEXT_REFINED_EVENT)
    );
}

#[tokio::test]
async fn retry_atomically_records_safe_correction_and_requeues_normal_bot_path() {
    let (state, draft_id, revision) = seeded_state();
    let app = dashboard_router(state.clone());
    let uri = format!("/api/accounts/{ACCOUNT}/drafts/{draft_id}/context-refinement/retry");
    let (status, body, raw) = request_json(
        &app,
        Method::POST,
        &uri,
        Some(retry_body(revision, &["informational"])),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "governed_retry_queued");
    assert_eq!(body["revision"], revision);
    assert!(body["send_after"].is_string());
    assert!(!raw.contains(RECIPIENT_SENTINEL));
    assert!(!raw.contains(SUBJECT_SENTINEL));
    assert!(!raw.contains(BODY_SENTINEL));
    assert!(!raw.contains(ATTACHMENT_SENTINEL));

    let db = state.db.lock().await;
    let queued = db.get_draft(&draft_id).unwrap().unwrap();
    assert_eq!(queued.status, DraftStatus::Draft);
    assert!(queued.send_after.is_some());
    assert_eq!(queued.created_by.as_deref(), Some("agent"));
    assert!(!queued.human_approved());
    assert_eq!(queued.human_send_surface(), None);
    assert!(queued.sent_at.is_none(), "retry must not transmit inline");
    assert_eq!(
        queued.metadata.as_ref().unwrap()["attribution"]["origin"],
        "bot"
    );
    assert_eq!(
        queued.metadata.as_ref().unwrap()["attribution"]["declared_attrs"],
        json!(["informational"])
    );
    assert!(
        queued
            .metadata
            .as_ref()
            .unwrap()
            .get("send_block")
            .is_none()
    );

    let correction = db
        .current_context_correction(ACCOUNT, &draft_id, revision)
        .unwrap()
        .unwrap();
    assert_eq!(correction.source, "dashboard");
    assert_eq!(correction.action, "send");
    assert_eq!(correction.declarable_attributes, ["informational"]);
    let event = db
        .list_events(Some(ACCOUNT), 100)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == CONTEXT_REFINED_EVENT)
        .unwrap();
    let audit = event.payload.unwrap();
    for private in [
        RECIPIENT_SENTINEL,
        SUBJECT_SENTINEL,
        BODY_SENTINEL,
        ATTACHMENT_SENTINEL,
        BYTES_SENTINEL,
        "private-free-text-title-must-not-project",
        "private-free-text-explanation-must-not-project",
    ] {
        assert!(!audit.contains(private), "audit leaked {private}");
    }
    assert!(audit.contains("informational"));
    assert!(!audit.contains("calendar_invitation"));
}
