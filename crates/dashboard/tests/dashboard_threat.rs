// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Threat engine through the real dashboard router: the draft-upload
// chokepoint refuses malware bytes with the stable `attachment_blocked` code,
// and the banner's verdict endpoint reads only the local store (no IMAP).
// Mark safe reads the message's bytes from IMAP before marking, so these
// tests drive it through `mark_safe_message` with the bytes, and through the
// router only where it answers before any IMAP connection.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::handlers::threat::mark_safe_message;
use envelope_email_dashboard::state::AppState;
use envelope_email_store::models::IndexedMessageInput;
use envelope_email_store::{CredentialBackend, Database, Draft};
use envelope_email_transport::threat::persist::{self, VerdictTarget};
use envelope_email_transport::threat::{self, Signal, ThreatConfig, combine, content_fingerprint};
use tower::ServiceExt;

/// The message the fixture's dangerous verdict at INBOX UID 7 judged.
const PHISH: &[u8] = b"Message-ID: <phish@x>\r\nFrom: Billing <billing@examp1e.org>\r\n\
To: me@example.org\r\nSubject: invoice\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n\
--b\r\nContent-Type: application/octet-stream\r\n\
Content-Disposition: attachment; filename=\"invoice.pdf.exe\"\r\n\r\nMZ\r\n--b--\r\n";

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
            content_fingerprint: content_fingerprint(PHISH).as_deref(),
            observed_message_ids: &[],
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

/// Mark safe the bytes at INBOX `uid`, as the handler does after fetching them.
async fn mark_bytes_safe(
    db: &tokio::sync::Mutex<Database>,
    uid: u32,
    raw: &[u8],
) -> (StatusCode, serde_json::Value) {
    let response = mark_safe_message(&*db.lock().await, "acc1", "INBOX", uid, raw);
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
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
async fn banner_verdict_uses_only_the_local_store_and_mark_safe_marks_its_bytes() {
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

    let (status, body) = mark_bytes_safe(&db, 7, PHISH).await;
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

    // No verdict at UID 8: refused before any IMAP connection.
    let token = mint_csrf(&app).await;
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

#[tokio::test]
async fn mark_safe_finds_the_verdict_when_the_message_has_a_new_uid() {
    // Scanned at INBOX UID 7; moved back, or delivered again, as UID 9 (#188).
    let (state, _) = state();
    let db = state.db.clone();
    db.lock()
        .await
        .upsert_indexed_message_summaries(
            "acc1",
            "INBOX",
            1,
            &[IndexedMessageInput {
                uid: 9,
                message_id: Some("<phish@x>".into()),
                from_addr: "billing@examp1e.org".into(),
                to_addr: "me@example.org".into(),
                subject: "invoice".into(),
                date: None,
                flags: vec![],
                size: 1,
                snippet: None,
                thread_id: None,
            }],
        )
        .unwrap();
    let app = dashboard_router(state);
    let token = mint_csrf(&app).await;

    // The Message-ID alone no longer finds a verdict for UID 9.
    let (status, body) = send(
        &app,
        "GET",
        "/api/accounts/acc1/messages/9/threat?folder=INBOX",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["threat"].is_null(), "{body}");
    let (status, body) = send(
        &app,
        "POST",
        "/api/accounts/acc1/messages/9/threat/mark-safe?folder=INBOX",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "not_scanned");

    // Opening UID 9 matches its bytes to the verdict scanned at UID 7.
    let opened = envelope_email_dashboard::handlers::threat::verdict_for_open(
        &*db.lock().await,
        "acc1",
        "me@example.org",
        "INBOX",
        9,
        persist::Opened::Whole(PHISH),
        &ThreatConfig::default(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(opened["level"], "dangerous", "{opened}");

    let (status, body) = mark_bytes_safe(&db, 9, PHISH).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "marked_safe");
    assert_eq!(body["threat"]["marked_safe"], true);

    let (status, body) = send(
        &app,
        "GET",
        "/api/accounts/acc1/messages/9/threat?folder=INBOX",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["threat"]["level"], "dangerous", "{body}");
    assert_eq!(body["threat"]["marked_safe"], true);
}

#[tokio::test]
async fn mark_safe_binds_to_the_bytes_at_that_uid() {
    let (state, _) = state();
    let db = state.db.clone();
    let app = dashboard_router(state);
    let fp = content_fingerprint(PHISH).unwrap();
    let resend = String::from_utf8_lossy(PHISH)
        .replace("see attached", "pay today")
        .into_bytes();
    let third = String::from_utf8_lossy(PHISH)
        .replace("see attached", "final notice")
        .into_bytes();

    // UID 8 holds another message with <phish@x>; opening it scans it.
    envelope_email_dashboard::handlers::threat::verdict_for_open(
        &*db.lock().await,
        "acc1",
        "me@example.org",
        "INBOX",
        8,
        persist::Opened::Whole(&resend),
        &ThreatConfig::default(),
    )
    .unwrap()
    .unwrap();

    let (status, body) = mark_bytes_safe(&db, 7, PHISH).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let label = db
        .lock()
        .await
        .events_for_message("acc1", "label_applied", "phish@x", 1)
        .unwrap()
        .remove(0);
    let payload: serde_json::Value = serde_json::from_str(&label.payload.unwrap()).unwrap();
    assert_eq!(payload["content_fingerprint"], fp.as_str());
    assert_eq!((label.folder.as_str(), label.uid), ("INBOX", Some(7)));

    let banner = |uid: u32| {
        let app = app.clone();
        async move {
            let uri = format!("/api/accounts/acc1/messages/{uid}/threat?folder=INBOX");
            let (status, body) = send(&app, "GET", &uri, None, None).await;
            assert_eq!(status, StatusCode::OK);
            body["threat"].clone()
        }
    };
    assert_eq!(banner(7).await["marked_safe"], true);
    let other = banner(8).await;
    assert_eq!(other["marked_safe"], false, "{other}");
    assert_eq!(other["level"], "dangerous", "{other}");

    // Bytes no verdict judged cannot be marked: the one on file for UID 8
    // is for other content.
    let (status, body) = mark_bytes_safe(&db, 8, &third).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "rescan_required");
}

#[tokio::test]
async fn mark_safe_on_one_twin_leaves_the_other_flagged_in_the_banner() {
    let (state, _) = state();
    let db = state.db.clone();
    let app = dashboard_router(state);
    let clean: &[u8] = b"Message-ID: <twin@x>\r\nFrom: Alice <alice@partner.example>\r\n\
To: me@example.org\r\nSubject: Lunch\r\n\r\nThursday?\r\n";
    let open = |uid: u32, raw: &'static [u8]| {
        let db = db.clone();
        async move {
            envelope_email_dashboard::handlers::threat::verdict_for_open(
                &*db.lock().await,
                "acc1",
                "me@example.org",
                "INBOX",
                uid,
                persist::Opened::Whole(raw),
                &ThreatConfig::default(),
            )
            .unwrap()
            .unwrap()
        }
    };
    let twin = String::from_utf8_lossy(PHISH).replace("phish@x", "twin@x");
    let twin: &'static [u8] = Box::leak(twin.into_bytes().into_boxed_slice());
    open(11, clean).await;
    open(12, twin).await;

    let (status, body) = mark_bytes_safe(&db, 11, clean).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    for view in [
        open(12, twin).await,
        send(
            &app,
            "GET",
            "/api/accounts/acc1/messages/12/threat?folder=INBOX",
            None,
            None,
        )
        .await
        .1["threat"]
            .clone(),
    ] {
        assert_eq!(view["malware"], true, "{view}");
        assert_eq!(view["marked_safe"], false, "{view}");
        let tags = view["tags"].as_array().unwrap();
        assert!(tags.contains(&"threat:malware".into()), "{view}");
        assert!(!tags.contains(&"threat:false_positive".into()), "{view}");
    }
}

/// A verdict stored without a content fingerprint judged no known bytes, so
/// Mark safe on the bytes now at that UID needs a rescan first.
#[tokio::test]
async fn mark_safe_refuses_a_verdict_without_a_fingerprint() {
    let (state, _) = state();
    let db = state.db.clone();
    let raw = String::from_utf8_lossy(PHISH)
        .replace("<phish@x>", "<legacy@x>")
        .into_bytes();
    persist::record_verdict(
        &*db.lock().await,
        &VerdictTarget {
            account_id: "acc1",
            folder: "INBOX",
            uid: 9,
            message_id: Some("legacy@x"),
            content_fingerprint: None,
            observed_message_ids: &[],
        },
        &combine(vec![], vec![], vec![], false),
    )
    .unwrap();

    let (status, body) = mark_bytes_safe(&db, 9, &raw).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "rescan_required");
}
