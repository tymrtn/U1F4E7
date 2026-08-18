// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Integration tests for draft attachments through the real dashboard router:
// the browser's actual flow (mint CSRF → POST upload → GET download → DELETE),
// plus the invariant that no draft JSON route ever carries attachment bytes.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::{CredentialBackend, Database, Draft};
use tower::ServiceExt;

fn state() -> (AppState, Draft) {
    let db = Database::open_memory().unwrap();
    db.conn()
        .execute(
            "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
             imap_host, imap_port, encrypted_password)
             VALUES ('acc1', 'Spain Expat', 'editor@spainexpat.com', 'spainexpat.com',
                     'smtp.spainexpat.com', 587, 'imap.spainexpat.com', 993, 'encrypted')",
            [],
        )
        .unwrap();
    let draft = db
        .create_draft(
            "acc1",
            "alexander@example.test",
            Some("Expatriator CBI interview test cases"),
            Some("Hi Alexander,"),
            None,
            None,
            None,
            None,
            Some("agent"),
        )
        .unwrap();
    (AppState::new(db, CredentialBackend::File), draft)
}

/// Mint a CSRF token the way the SPA does, returning the cookie+header value.
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
    assert_eq!(response.status(), StatusCode::OK);
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
) -> (StatusCode, Vec<u8>, Option<String>) {
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
    let disposition = response
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes, disposition)
}

fn as_json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap()
}

/// The full operator round trip, over the real router and middleware stack.
#[tokio::test]
async fn attach_download_and_detach_round_trip_through_the_router() {
    let (state, draft) = state();
    let app = dashboard_router(state);
    let token = mint_csrf(&app).await;
    let base = format!("/api/accounts/acc1/drafts/{}", draft.id);

    // Attach.
    let (status, body, _) = send(
        &app,
        "POST",
        &format!("{base}/attachments"),
        Some(&token),
        Some(serde_json::json!({
            "expected_revision": draft.revision,
            "attachments": [{
                "filename": "case-one.md",
                "content_type": "text/markdown",
                "data_b64": B64.encode(b"# Clean qualified family lead"),
            }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let attached = as_json(&body);
    let revision = attached["draft"]["revision"].as_i64().unwrap();
    assert_eq!(revision, draft.revision + 1);
    assert_eq!(
        attached["draft"]["attachments"][0]["filename"],
        "case-one.md"
    );

    // The list surface now shows the file too — with metadata, not bytes.
    let (status, body, _) = send(&app, "GET", &base, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let shown = as_json(&body);
    assert_eq!(shown["draft"]["attachments"][0]["size"], 29);

    // Download.
    let (status, bytes, disposition) = send(
        &app,
        "GET",
        &format!("{base}/attachments/case-one.md"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"# Clean qualified family lead");
    assert_eq!(
        disposition.as_deref(),
        Some("attachment; filename=\"case-one.md\"")
    );

    // Detach.
    let (status, body, _) = send(
        &app,
        "DELETE",
        &format!("{base}/attachments/case-one.md?expected_revision={revision}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        as_json(&body)["draft"]["attachments"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // And the download is gone with it.
    let (status, _, _) = send(
        &app,
        "GET",
        &format!("{base}/attachments/case-one.md"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Attachment bytes reach the browser through the download route alone. Every
/// JSON route that carries a draft must strip `data_base64` — before this the
/// review page downloaded every attachment in full on each load and rendered a
/// count.
#[tokio::test]
async fn no_draft_json_route_echoes_attachment_bytes() {
    let (state, draft) = state();
    let app = dashboard_router(state);
    let token = mint_csrf(&app).await;
    let base = format!("/api/accounts/acc1/drafts/{}", draft.id);
    let secret = B64.encode(b"classified test data");

    let (status, _, _) = send(
        &app,
        "POST",
        &format!("{base}/attachments"),
        Some(&token),
        Some(serde_json::json!({
            "expected_revision": draft.revision,
            "attachments": [{
                "filename": "case-two.md",
                "content_type": "text/markdown",
                "data_b64": secret,
            }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for uri in [base.clone(), "/api/accounts/acc1/drafts".to_string()] {
        let (status, body, _) = send(&app, "GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("case-two.md"),
            "{uri} must still list the attachment"
        );
        assert!(
            !text.contains("data_base64"),
            "{uri} must not echo the data_base64 field"
        );
        assert!(!text.contains(&secret), "{uri} must not echo the bytes");
    }
}

/// The mutations are edits, so they sit behind the same CSRF gate as an edit.
#[tokio::test]
async fn attachment_mutations_require_csrf() {
    let (state, draft) = state();
    let app = dashboard_router(state);
    let base = format!("/api/accounts/acc1/drafts/{}", draft.id);

    let (status, _, _) = send(
        &app,
        "POST",
        &format!("{base}/attachments"),
        None,
        Some(serde_json::json!({
            "expected_revision": draft.revision,
            "attachments": [{
                "filename": "x.txt",
                "content_type": "text/plain",
                "data_b64": B64.encode(b"x"),
            }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, _) = send(
        &app,
        "DELETE",
        &format!(
            "{base}/attachments/x.txt?expected_revision={}",
            draft.revision
        ),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
