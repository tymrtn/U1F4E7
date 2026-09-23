// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Integration test for POST /api/accounts/{id}/rules/{rule_id}/preview. A
// stored rule whose match has an empty condition list must be refused with a
// stable JSON error before any IMAP or credential-store work. This file holds
// a single test so it can point ENVELOPE_HOME at a temp dir for the whole
// process: a regression that reaches the credential store lands there, never
// in the real one.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database};
use envelope_email_transport::rules::EMPTY_CONDITION_LIST_SKIP_REASON;
use tower::ServiceExt;

#[tokio::test]
async fn preview_refuses_a_stored_empty_condition_rule_before_imap() {
    let home = tempfile::tempdir().expect("temp ENVELOPE_HOME");
    // SAFETY: this is the only test in this binary and it sets the variable
    // before anything else runs, so no other thread reads the environment.
    unsafe { std::env::set_var("ENVELOPE_HOME", home.path()) };

    let db = Database::open_memory().unwrap();
    let rule = db
        .create_rule("acc1", "oops", r#"{"and":[]}"#, r#""delete""#, 100, false)
        .unwrap();
    let app = dashboard_router(AppState::new(db, CredentialBackend::File));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/accounts/acc1/rules/{}/preview", rule.id))
                .header("content-type", "application/json")
                .header("cookie", "envelope_csrf=tok123")
                .header("x-envelope-csrf", "tok123")
                .body(Body::from(r#"{"folder":"INBOX","limit":50}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("body was not JSON: {}", String::from_utf8_lossy(&bytes)));
    assert_eq!(body["code"], "empty_match_condition");
    assert_eq!(body["error"], EMPTY_CONDITION_LIST_SKIP_REASON);
    assert!(
        !home.path().join("envelope-email").exists(),
        "preview must not touch the credential store"
    );
}
