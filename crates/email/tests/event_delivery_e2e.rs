// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)
//
// End-to-end tests for the durable event-delivery executor. A minimal HTTP
// server is spawned on a loopback port (no external network) that records each
// received request (headers + body) and replies with a scripted status code.
// This lets us assert the wire contract — signed headers, response capture,
// backoff, dead-lettering, and retry — against real reqwest traffic.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{TimeZone, Utc};
use envelope_email_store::Database;
use envelope_email_store::event_deliveries::DeliveryStatusFilter;
use envelope_email_store::models::Event;
use envelope_email_transport::event_delivery::{
    DeliveryLimits, MAX_ATTEMPTS, deliver_due_events, hmac_sha256_hex,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// One request captured by the mock server.
#[derive(Clone, Default)]
struct Captured {
    headers: String,
    body: String,
}

struct MockServer {
    url: String,
    captured: Arc<Mutex<Vec<Captured>>>,
    status: Arc<AtomicUsize>,
    response_body: Arc<Mutex<String>>,
}

impl MockServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/hook");
        let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(AtomicUsize::new(200));
        let response_body = Arc::new(Mutex::new("ok".to_string()));

        let cap = captured.clone();
        let st = status.clone();
        let rb = response_body.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let cap = cap.clone();
                let st = st.clone();
                let rb = rb.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                    let (headers, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
                    cap.lock().await.push(Captured {
                        headers: headers.to_string(),
                        body: body.to_string(),
                    });
                    let code = st.load(Ordering::SeqCst);
                    let reason = if (200..300).contains(&code) {
                        "OK"
                    } else {
                        "ERR"
                    };
                    let body = rb.lock().await.clone();
                    let resp = format!(
                        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });

        MockServer {
            url,
            captured,
            status,
            response_body,
        }
    }

    fn set_status(&self, code: u16) {
        self.status.store(code as usize, Ordering::SeqCst);
    }

    async fn set_response_body(&self, body: &str) {
        *self.response_body.lock().await = body.to_string();
    }

    async fn requests(&self) -> Vec<Captured> {
        self.captured.lock().await.clone()
    }
}

/// Build a DB with an account event, a webhook route, and one enqueued delivery.
/// Returns (db, route_secret, delivery_id, event_id).
fn seed(db: &Database, url: &str) -> (String, String, String) {
    let event = Event {
        id: "evt-1".to_string(),
        account_id: "acc-1".to_string(),
        event_type: "new_message".to_string(),
        folder: "INBOX".to_string(),
        uid: Some(7),
        message_id: Some("<m@x>".to_string()),
        from_addr: Some("a@x".to_string()),
        subject: Some("hi".to_string()),
        snippet: Some("hello".to_string()),
        payload: None,
        idempotency_key: Some("k1".to_string()),
        secure_pending: false,
        acked_at: None,
        created_at: "2026-07-08T00:00:00".to_string(),
    };
    db.insert_event(&event).unwrap();

    let delivery_spec = format!(r#"{{"type":"webhook","url":"{url}"}}"#);
    let route = db
        .create_event_route(
            "acc-1",
            r#"{"event_types":["new_message"]}"#,
            &delivery_spec,
            true,
            100,
        )
        .unwrap();
    let secret = route.secret.clone().unwrap();

    db.enqueue_delivery("del-1", "evt-1", &route.id, "d1", "2026-07-08T00:00:00")
        .unwrap();

    (secret, "del-1".to_string(), route.id)
}

#[tokio::test]
async fn success_records_delivered_at_snippet_and_signs_body() {
    let server = MockServer::start().await;
    server.set_response_body("received-thanks").await;
    let db = Database::open_memory().unwrap();
    let (secret, delivery_id, _route) = seed(&db, &server.url);

    let http = reqwest::Client::new();
    let now = Utc.with_ymd_and_hms(2026, 7, 8, 1, 0, 0).unwrap();
    let report = deliver_due_events(&db, &http, now, DeliveryLimits::default())
        .await
        .unwrap();

    assert_eq!(report.delivered, 1, "one delivery should succeed");
    assert_eq!(report.dead_lettered, 0);

    let d = db.get_delivery(&delivery_id).unwrap().unwrap();
    assert!(d.delivered_at.is_some(), "delivered_at must be stamped");
    assert_eq!(d.last_status_code, Some(200));
    assert_eq!(d.last_response_snippet.as_deref(), Some("received-thanks"));
    assert_eq!(d.attempt_count, 1);

    // The request carried the catalog headers and a valid HMAC over the body.
    let reqs = server.requests().await;
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert!(
        req.headers.contains("x-envelope-event: new_message")
            || req
                .headers
                .to_lowercase()
                .contains("x-envelope-event: new_message")
    );
    assert!(
        req.headers
            .to_lowercase()
            .contains("x-envelope-delivery: del-1")
    );

    // Recompute the signature over the exact received body and compare.
    let expected = format!(
        "sha256={}",
        hmac_sha256_hex(secret.as_bytes(), req.body.as_bytes())
    );
    let sig_line = req
        .headers
        .lines()
        .find(|l| l.to_lowercase().starts_with("x-envelope-signature:"))
        .expect("signature header present");
    let got = sig_line.splitn(2, ':').nth(1).unwrap().trim();
    assert_eq!(got, expected, "HMAC-SHA256 signature must match the body");
}

#[tokio::test]
async fn server_error_schedules_backoff_and_increments_attempts() {
    let server = MockServer::start().await;
    server.set_status(500);
    let db = Database::open_memory().unwrap();
    let (_secret, delivery_id, _route) = seed(&db, &server.url);

    let http = reqwest::Client::new();
    let now = Utc.with_ymd_and_hms(2026, 7, 8, 1, 0, 0).unwrap();
    let report = deliver_due_events(&db, &http, now, DeliveryLimits::default())
        .await
        .unwrap();

    assert_eq!(report.retried, 1, "a 500 reschedules rather than delivers");
    assert_eq!(report.delivered, 0);
    assert_eq!(report.dead_lettered, 0);

    let d = db.get_delivery(&delivery_id).unwrap().unwrap();
    assert_eq!(d.attempt_count, 1);
    assert!(d.delivered_at.is_none());
    assert!(d.dead_lettered_at.is_none());
    assert_eq!(d.last_status_code, Some(500));
    // First failure schedules the next attempt 60s out (backoff[0]).
    let next = d.next_attempt_at.expect("backoff schedules a retry");
    let parsed = chrono::DateTime::parse_from_rfc3339(&next).unwrap();
    assert_eq!(
        parsed.with_timezone(&Utc),
        now + chrono::Duration::seconds(60)
    );
}

#[tokio::test]
async fn repeated_failures_dead_letter_after_max_attempts() {
    let server = MockServer::start().await;
    server.set_status(503);
    let db = Database::open_memory().unwrap();
    let (_secret, delivery_id, _route) = seed(&db, &server.url);

    let http = reqwest::Client::new();

    // Drive one attempt per backoff step, advancing the clock past each
    // scheduled next_attempt_at so the delivery is due again.
    // MAX_ATTEMPTS backed-off retries follow the initial attempt, so the
    // delivery dead-letters on the (MAX_ATTEMPTS + 1)-th failure.
    let total_failures = MAX_ATTEMPTS + 1;
    let mut now = Utc.with_ymd_and_hms(2026, 7, 8, 1, 0, 0).unwrap();
    for _ in 0..total_failures {
        deliver_due_events(&db, &http, now, DeliveryLimits::default())
            .await
            .unwrap();
        // Jump the clock a full day so the next attempt is always due.
        now += chrono::Duration::days(1);
    }

    let d = db.get_delivery(&delivery_id).unwrap().unwrap();
    assert_eq!(d.attempt_count as u32, total_failures);
    assert!(
        d.dead_lettered_at.is_some(),
        "delivery must dead-letter after {MAX_ATTEMPTS} failures"
    );
    assert!(
        d.next_attempt_at.is_none(),
        "dead-lettered rows are not rescheduled"
    );

    // It no longer appears in the due set.
    let due = db.list_due_deliveries(&now.to_rfc3339(), 50).unwrap();
    assert!(due.iter().all(|x| x.id != delivery_id));
    let dead = db.list_deliveries(DeliveryStatusFilter::Dead, 50).unwrap();
    assert_eq!(dead.len(), 1);
}

#[tokio::test]
async fn retry_clears_dead_letter_and_redelivers() {
    let server = MockServer::start().await;
    server.set_status(500);
    let db = Database::open_memory().unwrap();
    let (_secret, delivery_id, _route) = seed(&db, &server.url);

    let http = reqwest::Client::new();
    let mut now = Utc.with_ymd_and_hms(2026, 7, 8, 1, 0, 0).unwrap();
    for _ in 0..(MAX_ATTEMPTS + 1) {
        deliver_due_events(&db, &http, now, DeliveryLimits::default())
            .await
            .unwrap();
        now += chrono::Duration::days(1);
    }
    assert!(
        db.get_delivery(&delivery_id)
            .unwrap()
            .unwrap()
            .dead_lettered_at
            .is_some()
    );

    // Operator retries; the server now returns 200.
    assert!(db.reset_delivery_for_retry(&delivery_id).unwrap());
    let after_reset = db.get_delivery(&delivery_id).unwrap().unwrap();
    assert!(
        after_reset.dead_lettered_at.is_none(),
        "retry clears dead-letter"
    );
    assert!(
        after_reset.next_attempt_at.is_none(),
        "retry clears backoff"
    );

    server.set_status(200);
    let report = deliver_due_events(&db, &http, now, DeliveryLimits::default())
        .await
        .unwrap();
    assert_eq!(report.delivered, 1);
    assert!(
        db.get_delivery(&delivery_id)
            .unwrap()
            .unwrap()
            .delivered_at
            .is_some()
    );
}
