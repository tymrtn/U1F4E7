// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Mailbox sync contract (#171), through the real dashboard router over an
// in-memory DB. The provider is an injected fake syncer that writes the index
// the way the read-only IMAP syncer does, so these tests open no sockets and
// never touch a credential store or a live mailbox. The IMAP syncer's
// read-only call set is pinned by `mailbox_sync::tests`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use envelope_email_dashboard::dashboard_router;
use envelope_email_dashboard::events::DashboardEvent;
use envelope_email_dashboard::mailbox_sync::{AccountSyncer, SyncLimits, SyncTarget};
use envelope_email_dashboard::state::AppState;
use envelope_email_store::models::IndexedMessageInput;
use envelope_email_store::{CredentialBackend, Database};
use futures_util::FutureExt;
use serde_json::Value;
use tower::ServiceExt;

fn msg(uid: u32, subject: &str, seen: bool) -> IndexedMessageInput {
    IndexedMessageInput {
        uid,
        message_id: Some(format!("<{subject}@example.test>")),
        from_addr: "sender@example.test".into(),
        to_addr: "me@example.test".into(),
        subject: subject.into(),
        date: Some(format!("Mon, 24 Aug 2026 10:{:02}:00 +0000", uid % 60)),
        flags: if seen { vec!["\\Seen".into()] } else { vec![] },
        size: 1,
        snippet: None,
        thread_id: None,
    }
}

fn insert_account(db: &Database, id: &str, username: &str) {
    db.conn()
        .execute(
            "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password)
             VALUES (?1, 'Test', ?2, 'example.test', 'smtp.example.test', 587, 'imap.example.test', 993, 'x')",
            [id, username],
        )
        .unwrap();
}

/// What the fake provider does for one account's sync.
#[derive(Clone)]
enum Provider {
    /// Replace the mailbox with these rows at this UIDVALIDITY.
    Rows(u64, Vec<IndexedMessageInput>),
    Fail(&'static str),
    Hang,
}

#[derive(Clone, Default)]
struct FakeProvider {
    behavior: Arc<Mutex<HashMap<(String, SyncTarget), Provider>>>,
    calls: Arc<Mutex<Vec<(String, SyncTarget)>>>,
    delay: Duration,
}

impl FakeProvider {
    fn set(&self, account: &str, target: SyncTarget, provider: Provider) {
        self.behavior
            .lock()
            .unwrap()
            .insert((account.to_string(), target), provider);
    }

    /// An unchanged mailbox: a sync rewrites the same rows.
    fn default_to(&self, account: &str, target: SyncTarget, rows: Vec<IndexedMessageInput>) {
        self.behavior
            .lock()
            .unwrap()
            .entry((account.to_string(), target))
            .or_insert(Provider::Rows(1, rows));
    }

    fn calls(&self) -> Vec<(String, SyncTarget)> {
        self.calls.lock().unwrap().clone()
    }

    fn syncer(&self) -> AccountSyncer {
        let fake = self.clone();
        Arc::new(move |state: AppState, account, target, _limit| {
            let fake = fake.clone();
            async move {
                fake.calls
                    .lock()
                    .unwrap()
                    .push((account.id.clone(), target));
                tokio::time::sleep(fake.delay).await;
                let behavior = fake
                    .behavior
                    .lock()
                    .unwrap()
                    .get(&(account.id.clone(), target))
                    .cloned()
                    .unwrap_or(Provider::Rows(1, vec![]));
                match behavior {
                    Provider::Rows(uidvalidity, rows) => {
                        let folder = match target {
                            SyncTarget::Inbox => "INBOX".to_string(),
                            SyncTarget::Sent => "Sent".to_string(),
                        };
                        let db = state.db.lock().await;
                        db.upsert_indexed_message_summaries(
                            &account.id,
                            &folder,
                            uidvalidity,
                            &rows,
                        )
                        .map_err(|e| e.to_string())
                    }
                    Provider::Fail(error) => Err(error.to_string()),
                    Provider::Hang => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        Ok(())
                    }
                }
            }
            .boxed()
        })
    }
}

/// Two accounts with cached INBOX and Sent rows indexed an hour ago.
fn seeded(fake: &FakeProvider) -> AppState {
    let db = Database::open_memory().unwrap();
    insert_account(&db, "acct-a", "a@example.test");
    insert_account(&db, "acct-b", "b@example.test");
    for (account, subject) in [("acct-a", "a-cached"), ("acct-b", "b-cached")] {
        let inbox = vec![msg(1, subject, false)];
        let sent = vec![msg(2, &format!("{subject}-sent"), true)];
        db.upsert_indexed_message_summaries(account, "INBOX", 1, &inbox)
            .unwrap();
        db.set_detected_folder(account, "sent", "Sent").unwrap();
        db.upsert_indexed_message_summaries(account, "Sent", 1, &sent)
            .unwrap();
        fake.default_to(account, SyncTarget::Inbox, inbox);
        fake.default_to(account, SyncTarget::Sent, sent);
    }
    let hour_ago = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    db.conn()
        .execute(
            "UPDATE message_index_state SET indexed_at = ?1",
            [&hour_ago],
        )
        .unwrap();
    db.conn()
        .execute(
            "UPDATE indexed_message_summaries SET indexed_at = ?1",
            [&hour_ago],
        )
        .unwrap();
    AppState::new(db, CredentialBackend::File)
        .with_syncer(fake.syncer())
        .with_sync_limits(SyncLimits {
            concurrency: 4,
            account_timeout: Duration::from_millis(400),
        })
}

async fn call(state: &AppState, method: &str, uri: &str) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if method == "POST" {
        // Open mode + matching double-submit CSRF token.
        request = request
            .header("cookie", "envelope_csrf=tok123")
            .header("x-envelope-csrf", "tok123");
    }
    let response = dashboard_router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Subjects in the response, sorted (order is covered by the index tests).
fn subjects(json: &Value) -> Vec<String> {
    let mut subjects: Vec<String> = json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["subject"].as_str().unwrap().to_string())
        .collect();
    subjects.sort();
    subjects
}

fn account<'a>(json: &'a Value, id: &str) -> &'a Value {
    json["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["account_id"] == id)
        .unwrap_or_else(|| panic!("account {id} in response"))
}

fn message<'a>(json: &'a Value, subject: &str) -> &'a Value {
    json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"] == subject)
        .unwrap_or_else(|| panic!("message {subject} in response"))
}

#[tokio::test]
async fn cached_get_first_paint_runs_no_provider_sync_and_opens_no_imap() {
    let fake = FakeProvider::default();
    let state = seeded(&fake);

    for uri in [
        "/api/messages/unified",
        "/api/messages/sent",
        "/api/cockpit",
    ] {
        let (status, json) = call(&state, "GET", uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(json.get("sync").is_none(), "{uri} is cache-only");
    }
    let (_, unified) = call(&state, "GET", "/api/messages/unified").await;
    assert_eq!(subjects(&unified), vec!["a-cached", "b-cached"]);
    assert!(unified["generated_at"].as_str().is_some());

    assert!(fake.calls().is_empty(), "GET never reaches the provider");
    assert!(
        state.imap_pool.lock().await.is_empty(),
        "no IMAP connection (and so no credential decrypt) on GET"
    );
    assert_eq!(state.sync_flights.in_flight(), 0);
}

#[tokio::test]
async fn sync_success_updates_rows_unread_flags_and_last_success_time() {
    let fake = FakeProvider::default();
    // A already-read cached message and a newly arrived unread one.
    fake.set(
        "acct-a",
        SyncTarget::Inbox,
        Provider::Rows(1, vec![msg(1, "a-cached", true), msg(3, "a-new", false)]),
    );
    let state = seeded(&fake);
    let (_, before) = call(&state, "GET", "/api/messages/unified").await;
    let indexed_before = account(&before, "acct-a")["indexed_at"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, json) = call(&state, "POST", "/api/messages/unified/refresh?limit=50").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["sync"]["status"], "ok");
    assert_eq!(json["sync"]["target"], "inbox");
    assert!(json["sync"]["account_id"].is_null());
    assert_eq!(message(&json, "a-new")["unread"], true);
    assert_eq!(message(&json, "a-cached")["unread"], false);
    // a-new unread + b-cached unread.
    assert_eq!(json["unread_count"], 2);
    let a = account(&json, "acct-a");
    assert_eq!(a["ok"], true);
    assert_eq!(a["freshness"], "fresh");
    assert!(a["indexed_at"].as_str().unwrap() > indexed_before.as_str());
    assert!(json["generated_at"].as_str().is_some());
}

#[tokio::test]
async fn one_provider_failure_keeps_that_accounts_cached_rows_as_stale_partial() {
    let fake = FakeProvider::default();
    fake.set(
        "acct-b",
        SyncTarget::Inbox,
        Provider::Fail("IMAP: auth failed"),
    );
    let state = seeded(&fake);

    let (status, json) = call(&state, "POST", "/api/messages/unified/refresh").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "partial");
    assert_eq!(json["freshness"], "partial");
    assert_eq!(json["sync"]["status"], "partial");
    // The failed account's cached row is still served, labeled stale.
    assert_eq!(message(&json, "b-cached")["index_freshness"], "stale");
    let b = account(&json, "acct-b");
    assert_eq!(b["ok"], false);
    assert_eq!(b["freshness"], "stale");
    assert_eq!(b["message_count"], 1);
    assert_eq!(b["error"], "IMAP: auth failed");
    assert_eq!(json["errors"][0]["account_id"], "acct-b");

    // The failure persists: the next cached GET still shows the rows, stale.
    let (_, cached) = call(&state, "GET", "/api/messages/unified").await;
    assert_eq!(subjects(&cached), vec!["a-cached", "b-cached"]);
    assert_eq!(account(&cached, "acct-b")["freshness"], "stale");
    assert_eq!(account(&cached, "acct-b")["error"], "IMAP: auth failed");
}

#[tokio::test]
async fn provider_timeout_is_bounded_and_reported_stale_not_empty() {
    let fake = FakeProvider::default();
    fake.set("acct-b", SyncTarget::Inbox, Provider::Hang);
    let state = seeded(&fake);

    let started = std::time::Instant::now();
    let (_, json) = call(&state, "POST", "/api/messages/unified/refresh").await;

    assert!(
        started.elapsed() < Duration::from_secs(3),
        "bounded by budget"
    );
    let b = account(&json, "acct-b");
    assert_eq!(b["freshness"], "stale");
    assert!(b["error"].as_str().unwrap().contains("timed out"));
    assert!(subjects(&json).contains(&"b-cached".to_string()));
    assert_eq!(account(&json, "acct-a")["ok"], true);
}

#[tokio::test]
async fn scoped_sync_touches_only_that_account_and_announces_only_it() {
    let fake = FakeProvider::default();
    let state = seeded(&fake);
    let mut events = state.events.subscribe();

    let (status, json) = call(
        &state,
        "POST",
        "/api/messages/unified/refresh?account_id=b@example.test",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        fake.calls(),
        vec![("acct-b".to_string(), SyncTarget::Inbox)]
    );
    assert_eq!(json["sync"]["account_id"], "acct-b");
    assert_eq!(json["sync"]["accounts"].as_array().unwrap().len(), 1);
    // The response is still the whole view, not just the synced account.
    assert!(subjects(&json).contains(&"a-cached".to_string()));

    let mut announced = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let DashboardEvent::NewMail { account_id, .. } = event {
            announced.push(account_id);
        }
    }
    assert_eq!(announced, vec!["acct-b".to_string()]);
}

#[tokio::test]
async fn unknown_scoped_account_is_404_and_syncs_nothing() {
    let fake = FakeProvider::default();
    let state = seeded(&fake);

    let (status, json) = call(
        &state,
        "POST",
        "/api/messages/unified/refresh?account_id=nobody@example.test",
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"], "account_not_found");
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn simultaneous_syncs_from_two_tabs_share_one_provider_run_per_account() {
    let fake = FakeProvider {
        delay: Duration::from_millis(150),
        ..FakeProvider::default()
    };
    let state = seeded(&fake);

    let (first, second) = tokio::join!(
        call(&state, "POST", "/api/messages/unified/refresh"),
        call(&state, "POST", "/api/messages/unified/refresh"),
    );

    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    let calls = fake.calls();
    assert_eq!(
        calls.len(),
        2,
        "one run per account, not per tab: {calls:?}"
    );
    let joined: usize = [&first.1, &second.1]
        .iter()
        .flat_map(|json| json["sync"]["accounts"].as_array().unwrap().iter())
        .filter(|a| a["joined"] == true)
        .count();
    assert_eq!(joined, 2, "the second tab joined both account runs");
    assert_eq!(first.1["sync"]["status"], "ok");
    assert_eq!(second.1["sync"]["status"], "ok");
}

#[tokio::test]
async fn uidvalidity_reset_and_deleted_messages_leave_no_duplicate_or_stale_rows() {
    let fake = FakeProvider::default();
    // Server reset UIDVALIDITY; UID 1 is now a different message and the old
    // one is gone.
    fake.set(
        "acct-a",
        SyncTarget::Inbox,
        Provider::Rows(2, vec![msg(1, "a-after-reset", false)]),
    );
    let state = seeded(&fake);

    let (_, json) = call(&state, "POST", "/api/messages/unified/refresh").await;

    let a_rows: Vec<&Value> = json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["account_id"] == "acct-a")
        .collect();
    assert_eq!(a_rows.len(), 1);
    assert_eq!(a_rows[0]["subject"], "a-after-reset");
    assert_eq!(a_rows[0]["uidvalidity"], 2);
}

#[tokio::test]
async fn sent_sync_failure_keeps_cached_sent_rows_stale_and_scopes_to_sent() {
    let fake = FakeProvider::default();
    fake.set(
        "acct-a",
        SyncTarget::Sent,
        Provider::Fail("resolve sent folder: timeout"),
    );
    let state = seeded(&fake);

    let (status, json) = call(&state, "POST", "/api/messages/sent/refresh").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["sync"]["target"], "sent");
    assert!(
        fake.calls()
            .iter()
            .all(|(_, target)| *target == SyncTarget::Sent)
    );
    assert_eq!(json["sync"]["status"], "partial");
    assert_eq!(message(&json, "a-cached-sent")["index_freshness"], "stale");
    let a = account(&json, "acct-a");
    assert_eq!(a["ok"], false);
    assert_eq!(a["freshness"], "stale");

    let (_, cached) = call(&state, "GET", "/api/messages/sent").await;
    assert!(subjects(&cached).contains(&"a-cached-sent".to_string()));
    assert_eq!(account(&cached, "acct-a")["freshness"], "stale");
}
