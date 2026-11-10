// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// A server that leaves the background jobs to another server on its home
// (the desktop app beside the installed dashboard) runs no sync of its own,
// and the other server's `new_mail` events stay in that server's process. It
// watches the shared index instead and announces what the other server's sync
// wrote, so its own event stream (the tray's new-mail alerts) still hears it.

use envelope_email_dashboard::events::DashboardEvent;
use envelope_email_dashboard::handlers::messages::{IndexWatch, announce_index_changes};
use envelope_email_dashboard::state::AppState;
use envelope_email_store::models::IndexedMessageInput;
use envelope_email_store::{CredentialBackend, Database};
use tokio::sync::broadcast::Receiver;

fn msg(uid: u32, seen: bool) -> IndexedMessageInput {
    IndexedMessageInput {
        uid,
        message_id: Some(format!("<m{uid}@example.test>")),
        from_addr: "sender@example.test".into(),
        to_addr: "me@example.test".into(),
        subject: format!("message {uid}"),
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

/// Two accounts, each with one read message indexed in INBOX.
fn seeded() -> AppState {
    let db = Database::open_memory().unwrap();
    for (id, user) in [("acct-a", "a@example.test"), ("acct-b", "b@example.test")] {
        insert_account(&db, id, user);
        db.upsert_indexed_message_summaries(id, "INBOX", 1, &[msg(1, true)])
            .unwrap();
    }
    AppState::new(db, CredentialBackend::File)
}

/// What the other server's sync writes when mail arrives for `account`: the
/// mailbox as it now stands, the old read message plus the new unread one.
async fn other_server_indexes(state: &AppState, account: &str, uid: u32) {
    state
        .db
        .lock()
        .await
        .upsert_indexed_message_summaries(account, "INBOX", 1, &[msg(1, true), msg(uid, false)])
        .unwrap();
}

fn announced(events: &mut Receiver<DashboardEvent>) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let DashboardEvent::NewMail {
            account_id,
            message_count,
            unread_count,
        } = event
        {
            out.push((account_id, message_count, unread_count));
        }
    }
    out
}

#[tokio::test]
async fn the_first_look_is_a_baseline_and_announces_nothing() {
    let state = seeded();
    let mut events = state.events.subscribe();
    let mut watch = IndexWatch::default();

    announce_index_changes(&state, &mut watch).await;

    assert_eq!(announced(&mut events), vec![]);
}

#[tokio::test]
async fn mail_the_other_server_indexed_is_announced_once_for_its_account() {
    let state = seeded();
    let mut events = state.events.subscribe();
    let mut watch = IndexWatch::default();
    announce_index_changes(&state, &mut watch).await;

    other_server_indexes(&state, "acct-b", 2).await;
    announce_index_changes(&state, &mut watch).await;
    assert_eq!(
        announced(&mut events),
        vec![("acct-b".to_string(), 2, 1)],
        "the same counts the sync's own announcement carries"
    );

    announce_index_changes(&state, &mut watch).await;
    assert_eq!(
        announced(&mut events),
        vec![],
        "an unchanged index is quiet"
    );
}

#[tokio::test]
async fn an_account_added_later_starts_from_a_baseline() {
    let state = seeded();
    let mut events = state.events.subscribe();
    let mut watch = IndexWatch::default();
    announce_index_changes(&state, &mut watch).await;

    {
        let db = state.db.lock().await;
        insert_account(&db, "acct-c", "c@example.test");
        db.upsert_indexed_message_summaries("acct-c", "INBOX", 1, &[msg(1, false)])
            .unwrap();
    }
    announce_index_changes(&state, &mut watch).await;

    assert_eq!(announced(&mut events), vec![]);
}
