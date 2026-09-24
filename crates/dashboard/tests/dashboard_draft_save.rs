// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Save draft from the dashboard composer: POST /api/accounts/{id}/drafts
// stores a local draft and queues nothing. It needs no IMAP connection.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database, DraftStatus};
use tower::ServiceExt;

fn state() -> AppState {
    let db = Database::open_memory().unwrap();
    db.conn()
        .execute(
            "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
             imap_host, imap_port, encrypted_password)
             VALUES ('acc1', 'Editor', 'editor@example.test', 'example.test',
                     'smtp.example.test', 587, 'imap.example.test', 993, 'encrypted')",
            [],
        )
        .unwrap();
    AppState::new(db, CredentialBackend::File)
}

async fn mint_csrf(app: &Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/csrf")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    json["token"].as_str().unwrap().to_string()
}

async fn post(
    app: &Router,
    uri: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-envelope-csrf", token)
        .header(header::COOKIE, format!("envelope_csrf={token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

#[tokio::test]
async fn save_draft_stores_an_unqueued_draft() {
    let state = state();
    let db = state.db.clone();
    let app = dashboard_router(state);
    let token = mint_csrf(&app).await;

    let (status, body) = post(
        &app,
        "/api/accounts/acc1/drafts",
        &token,
        serde_json::json!({
            "to": "reader@example.test",
            "subject": "Saved from compose",
            "text": "Half written.",
            "cc": "copy@example.test"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "draft");
    let draft_id = json["draft"]["id"].as_str().unwrap().to_string();
    assert_eq!(json["draft"]["created_by"], "human:dashboard");

    let db = db.lock().await;
    let draft = db.get_draft(&draft_id).unwrap().unwrap();
    assert_eq!(draft.status, DraftStatus::Draft);
    assert_eq!(draft.send_after, None, "a saved draft is not queued");
    assert_eq!(draft.to_addr, "reader@example.test");
    assert_eq!(draft.subject.as_deref(), Some("Saved from compose"));
    assert_eq!(draft.text_content.as_deref(), Some("Half written."));
    assert_eq!(draft.cc_addr.as_deref(), Some("copy@example.test"));
}

#[tokio::test]
async fn save_draft_for_unknown_account_is_404() {
    let app = dashboard_router(state());
    let token = mint_csrf(&app).await;
    let (status, _) = post(
        &app,
        "/api/accounts/nope/drafts",
        &token,
        serde_json::json!({ "to": "", "subject": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// Files go through the draft attachment route after the save, so they get the
// same size, filename and threat checks as any other draft attachment.
#[tokio::test]
async fn save_draft_refuses_inline_attachments() {
    let app = dashboard_router(state());
    let token = mint_csrf(&app).await;
    let (status, _) = post(
        &app,
        "/api/accounts/acc1/drafts",
        &token,
        serde_json::json!({
            "to": "reader@example.test",
            "subject": "x",
            "attachments": [{ "filename": "a", "content_type": "text/plain", "data_b64": "aGk=" }]
        }),
    )
    .await;
    assert!(status.is_client_error(), "got {status}");
}
