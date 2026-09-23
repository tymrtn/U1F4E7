// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Threat engine through the real dashboard router: the draft-upload
// chokepoint refuses malware bytes with the stable `attachment_blocked` code,
// and the banner's verdict / Mark safe endpoints read and write only the
// local store (no IMAP).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database, Draft};
use envelope_email_transport::threat::persist::{self, VerdictTarget};
use envelope_email_transport::threat::{self, Signal, combine};
use tower::ServiceExt;

fn state() -> (AppState, Draft) {
    let db = Database::open_memory().unwrap();
    db.conn()
        .execute(
            "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
             imap_host, imap_port, encrypted_password)
             VALUES ('acc1', 'Me', 'me@example.org', 'example.org',
                     'smtp.example.org', 587, 'imap.example.org', 993, 'encrypted')",
            [],
        )
        .unwrap();
    let draft = db
        .create_draft(
            "acc1",
            "someone@example.test",
            Some("files"),
            Some("see attached"),
            None,
            None,
            None,
            None,
            Some("human"),
        )
        .unwrap();
    let dangerous = combine(
        vec![
            Signal::new(
                "lookalike_domain",
                45,
                "sender domain examp1e.org imitates example.org",
            ),
            Signal::new("double_extension", 70, "ext=.pdf.exe sha256=00").malware(),
        ],
        vec!["sender".into(), "attachments".into()],
        vec![],
        false,
    );
    persist::record_verdict(
        &db,
        &VerdictTarget {
            account_id: "acc1",
            folder: "INBOX",
            uid: 7,
            message_id: Some("phish@x"),
        },
        &dangerous,
    )
    .unwrap();
    (AppState::new(db, CredentialBackend::File), draft)
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

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder
            .header("x-envelope-csrf", token)
            .header(header::COOKIE, format!("envelope_csrf={token}"));
    }
    let request = match body {
        Some(json) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&json).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
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
async fn draft_upload_refuses_malware_with_attachment_blocked() {
    let (state, draft) = state();
    let db = state.db.clone();
    let app = dashboard_router(state);
    let token = mint_csrf(&app).await;
    let uri = format!("/api/accounts/acc1/drafts/{}/attachments", draft.id);

    let (status, body) = send(
        &app,
        "POST",
        &uri,
        Some(&token),
        Some(serde_json::json!({
            "expected_revision": draft.revision,
            "attachments": [{
                "filename": "invoice.pdf.exe",
                "content_type": "application/pdf",
                "data_b64": B64.encode(b"MZ\x90\x00"),
            }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "attachment_blocked");
    assert_eq!(body["signals"][0]["code"], "double_extension");
    let stored = db.lock().await.get_draft(&draft.id).unwrap().unwrap();
    assert!(stored.attachments.is_empty(), "no bytes were stored");

    let (status, body) = send(
        &app,
        "POST",
        &uri,
        Some(&token),
        Some(serde_json::json!({
            "expected_revision": draft.revision,
            "attachments": [{
                "filename": "notes.pdf",
                "content_type": "application/pdf",
                "data_b64": B64.encode(b"%PDF-1.7"),
            }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn banner_verdict_and_mark_safe_use_only_the_local_store() {
    let (state, _) = state();
    let db = state.db.clone();
    let app = dashboard_router(state);

    let (status, body) = send(
        &app,
        "GET",
        "/api/accounts/acc1/messages/7/threat",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["threat"]["level"], "dangerous");
    assert_eq!(body["threat"]["score"], 100);
    assert_eq!(body["threat"]["malware"], true);
    assert!(
        body["threat"]["explain"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == "= 115, capped at 100")
    );

    let (status, body) = send(
        &app,
        "GET",
        "/api/accounts/acc1/messages/8/threat",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["threat"].is_null());

    let token = mint_csrf(&app).await;
    let (status, body) = send(
        &app,
        "POST",
        "/api/accounts/acc1/messages/7/threat/mark-safe",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["threat"]["marked_safe"], true);
    let tags: Vec<String> = db
        .lock()
        .await
        .get_tags("acc1", "phish@x")
        .unwrap()
        .into_iter()
        .map(|t| t.tag)
        .collect();
    assert_eq!(tags, vec![threat::TAG_FALSE_POSITIVE.to_string()]);

    let (status, body) = send(
        &app,
        "POST",
        "/api/accounts/acc1/messages/8/threat/mark-safe",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "not_scanned");
}
