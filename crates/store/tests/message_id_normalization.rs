// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Message-IDs are stored bare (`id@host`) in all three tables that key on
//! them, so joins across the message index, the event log and the thread
//! cache match. IMAP ENVELOPE hands us `<id@host>`; mail_parser hands us
//! `id@host`.

use envelope_email_store::Database;
use envelope_email_store::models::{Event, IndexedMessageInput};

fn db() -> (Database, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("envelope.db")).unwrap();
    (db, dir)
}

fn event(id: &str, message_id: &str) -> Event {
    Event {
        id: id.to_string(),
        account_id: "a1".to_string(),
        event_type: "message_seen".to_string(),
        folder: "INBOX".to_string(),
        uid: Some(7),
        message_id: Some(message_id.to_string()),
        from_addr: None,
        subject: None,
        snippet: None,
        payload: Some(format!(r#"{{"message_id":"{message_id}"}}"#)),
        idempotency_key: Some(format!("key-{id}")),
        secure_pending: false,
        acked_at: None,
        created_at: "2026-09-26T08:00:00Z".to_string(),
    }
}

fn index_one(db: &Database, message_id: &str) {
    db.upsert_indexed_message_summaries(
        "a1",
        "INBOX",
        1,
        &[IndexedMessageInput {
            uid: 7,
            message_id: Some(message_id.to_string()),
            from_addr: "sender@x.test".to_string(),
            to_addr: "me@x.test".to_string(),
            subject: "s".to_string(),
            date: Some("Sat, 26 Sep 2026 08:00:00 +0000".to_string()),
            flags: vec![],
            size: 1,
            snippet: None,
            thread_id: None,
        }],
    )
    .unwrap();
}

fn stored(db: &Database, sql: &str) -> Vec<Option<String>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn every_write_path_stores_the_bare_form_and_the_tables_join() {
    let (db, _dir) = db();
    index_one(&db, "<m1@x.test>");
    db.insert_event_with_agent(&event("e1", "<m1@x.test>"), None)
        .unwrap();
    db.insert_event_idempotent_with_agent(&event("e2", " <m1@x.test> "), None)
        .unwrap();
    let thread = db
        .create_thread("s", "2026-09-26T08:00:00Z", "2026-09-26T08:00:00Z", "a1")
        .unwrap();
    db.upsert_thread_message(
        &thread.thread_id,
        7,
        Some("<m1@x.test>"),
        Some("<p@x.test>"),
        Some(" <a@x.test>  <p@x.test> "),
        "INBOX",
        "sender@x.test",
        "me@x.test",
        None,
        None,
        "2026-09-26T08:00:00Z",
        "s",
        false,
        None,
    )
    .unwrap();

    let bare = Some("m1@x.test".to_string());
    assert_eq!(
        stored(&db, "SELECT message_id FROM indexed_message_summaries"),
        [bare.clone()]
    );
    assert_eq!(
        stored(&db, "SELECT message_id FROM events ORDER BY id"),
        [bare.clone(), bare.clone()]
    );
    assert_eq!(
        stored(&db, "SELECT message_id FROM thread_messages"),
        [bare.clone()]
    );
    assert_eq!(
        stored(&db, "SELECT in_reply_to FROM thread_messages"),
        [Some("p@x.test".to_string())]
    );
    assert_eq!(
        stored(&db, "SELECT reference_ids FROM thread_messages"),
        [Some("a@x.test p@x.test".to_string())]
    );

    let joined: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM indexed_message_summaries s
             JOIN events e ON e.message_id = s.message_id
             JOIN thread_messages tm ON tm.message_id = s.message_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        joined, 2,
        "both events join the index row and the thread row"
    );
}

#[test]
fn an_empty_bracket_pair_is_stored_as_no_message_id() {
    let (db, _dir) = db();
    index_one(&db, "<>");
    db.insert_event_with_agent(&event("e1", " <> "), None)
        .unwrap();
    assert_eq!(
        stored(&db, "SELECT message_id FROM indexed_message_summaries"),
        [None]
    );
    assert_eq!(stored(&db, "SELECT message_id FROM events"), [None]);
}

#[test]
fn the_repair_strips_brackets_left_by_older_writers_and_is_idempotent() {
    let (db, _dir) = db();
    // What public v1 and earlier v2 builds wrote, straight to the tables.
    db.conn()
        .execute_batch(
            "INSERT INTO indexed_message_summaries
                 (account_id, folder, uidvalidity, uid, message_id, indexed_at)
             VALUES ('a1', 'INBOX', 1, 7, '<m1@x.test>', '2026-09-26T08:00:00Z'),
                    ('a1', 'INBOX', 1, 8, 'm2@x.test', '2026-09-26T08:00:00Z'),
                    ('a1', 'INBOX', 1, 9, '<>', '2026-09-26T08:00:00Z');
             INSERT INTO events (id, account_id, event_type, folder, message_id, payload,
                                 idempotency_key, created_at)
             VALUES ('e1', 'a1', 'message_seen', 'INBOX', '<m1@x.test>',
                     '{\"message_id\":\"<m1@x.test>\"}', 'k1', '2026-09-26T08:00:00Z'),
                    ('e2', 'a1', 'message_opened', 'INBOX', 'm1@x.test', '{}', NULL,
                     '2026-09-26T08:00:00Z');",
        )
        .unwrap();

    let first = db.normalize_stored_message_ids().unwrap();
    assert_eq!(
        (first.index_rows, first.events, first.thread_messages),
        (2, 1, 0)
    );
    assert_eq!(
        stored(
            &db,
            "SELECT message_id FROM indexed_message_summaries ORDER BY uid"
        ),
        [
            Some("m1@x.test".to_string()),
            Some("m2@x.test".to_string()),
            None
        ]
    );
    assert_eq!(
        stored(&db, "SELECT message_id FROM events ORDER BY id"),
        [Some("m1@x.test".to_string()), Some("m1@x.test".to_string())]
    );
    // Only the column changes: the payload a webhook already carried, and the
    // idempotency key, stay exactly as written.
    assert_eq!(
        stored(&db, "SELECT payload FROM events WHERE id = 'e1'"),
        [Some(r#"{"message_id":"<m1@x.test>"}"#.to_string())]
    );
    assert_eq!(
        stored(&db, "SELECT idempotency_key FROM events WHERE id = 'e1'"),
        [Some("k1".to_string())]
    );

    let again = db.normalize_stored_message_ids().unwrap();
    assert_eq!(
        (again.index_rows, again.events, again.thread_messages),
        (0, 0, 0)
    );
}
