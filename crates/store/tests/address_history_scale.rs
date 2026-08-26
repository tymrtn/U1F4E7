// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Production-scale probe for the compose autocomplete address history.
//!
//! An established Envelope install holds six figures of `thread_messages`
//! rows across a couple of dozen accounts. That shape is what the watermark
//! and the chunked backfill exist for, and it is not something the unit tests
//! — which run on a handful of rows — can demonstrate. This builds a synthetic
//! database of that shape in a temp directory and measures the three claims
//! the design rests on:
//!
//!   1. the one-time backfill folds the whole thread cache in, in bounded
//!      chunks;
//!   2. a reconcile afterwards reads no thread rows, so a refresh is not a
//!      rescan;
//!   3. a suggestion query answers from `contacts` alone, never touching
//!      `thread_messages`, and does so at typing speed.
//!
//! Ignored by default because it writes ~150k rows: run it with
//! `cargo test -p envelope-email-store --test address_history_scale -- --ignored --nocapture`.

use std::time::Instant;

use envelope_email_store::{ADDRESS_HISTORY_CHUNK_ROWS, Database};

/// Mirrors the live database this feature was corrected against.
const ACCOUNTS: usize = 23;
const LARGEST_ACCOUNT_MESSAGES: usize = 90_000;
const OTHER_ACCOUNT_MESSAGES: usize = 2_500;
/// Recent-inbox snapshot rows per account, as the dashboard refresh writes.
const SUMMARIES_PER_ACCOUNT: usize = 32;
const SENT_DRAFTS_PER_ACCOUNT: usize = 4;

#[test]
#[ignore = "scale probe: writes ~150k rows, run explicitly"]
fn backfill_and_suggest_at_production_scale() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("envelope-scale.db");
    let db = Database::open(&path).unwrap();

    let seeded = Instant::now();
    let total_messages = seed(&db);
    println!(
        "seeded {total_messages} thread messages across {ACCOUNTS} accounts in {:?}",
        seeded.elapsed()
    );

    let accounts: Vec<String> = db
        .list_accounts()
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert_eq!(accounts.len(), ACCOUNTS);

    // ── 1. One-time backfill, in bounded chunks ───────────────────────
    let backfill = Instant::now();
    let mut chunks = 0usize;
    let mut folded = 0usize;
    let mut slowest_chunk = std::time::Duration::ZERO;
    for account in &accounts {
        loop {
            let started = Instant::now();
            let pass = db
                .reconcile_address_history_chunk(account, ADDRESS_HISTORY_CHUNK_ROWS)
                .unwrap();
            slowest_chunk = slowest_chunk.max(started.elapsed());
            chunks += 1;
            folded += pass.thread_rows;
            assert!(
                pass.thread_rows <= ADDRESS_HISTORY_CHUNK_ROWS,
                "a chunk must stay within its bound"
            );
            if !pass.pending {
                break;
            }
        }
    }
    let backfill_elapsed = backfill.elapsed();
    println!(
        "backfill: {folded} thread rows in {chunks} chunks, {backfill_elapsed:?} total, \
         slowest chunk {slowest_chunk:?}"
    );
    assert_eq!(folded, total_messages, "every thread row is folded in once");

    // ── 2. A later reconcile is not a rescan ──────────────────────────
    let steady = Instant::now();
    let mut reread = 0usize;
    let mut swept = 0usize;
    for account in &accounts {
        let pass = db.reconcile_address_history(account).unwrap();
        reread += pass.thread_rows;
        swept += pass.removed;
    }
    let steady_elapsed = steady.elapsed();
    println!("caught-up reconcile of all {ACCOUNTS} accounts: {steady_elapsed:?}");
    assert_eq!(reread, 0, "a caught-up reconcile must read no thread rows");
    assert_eq!(
        swept, 0,
        "every contact still has a source: a caught-up reconcile deletes none of them"
    );

    // New mail costs the new mail, not the cache.
    let biggest = &accounts[0];
    append_thread_message(
        &db,
        biggest,
        999_001,
        "brand-new@late.test",
        "account-000@example.test",
        "2026-08-15T09:00:00Z",
        false,
    );
    let incremental = Instant::now();
    let pass = db.reconcile_address_history(biggest).unwrap();
    println!(
        "incremental reconcile after 1 new message: {:?} ({} thread rows)",
        incremental.elapsed(),
        pass.thread_rows
    );
    assert_eq!(pass.thread_rows, 1);
    assert_eq!(
        db.suggest_addresses(biggest, "brand-new", 8).unwrap().len(),
        1,
        "new mail reaches autocomplete without a rebuild"
    );

    // ── 3. Typing reads contacts, and only contacts ───────────────────
    let plan = db.suggestion_query_plan(biggest, "peer").unwrap();
    for table in ["thread_messages", "threads", "indexed_message_summaries"] {
        assert!(
            !plan.contains(table),
            "a suggestion query must not touch {table}: {plan}"
        );
    }
    println!("suggestion query plan: {plan}");

    let contacts = db.list_contacts(biggest, None).unwrap().len();
    let mut slowest_query = std::time::Duration::ZERO;
    for needle in ["a", "ac", "acc", "peer", "peer-1", "vendor", "zzz-nobody"] {
        let started = Instant::now();
        let rows = db.suggest_addresses(biggest, needle, 8).unwrap();
        let elapsed = started.elapsed();
        slowest_query = slowest_query.max(elapsed);
        println!("  q={needle:<10} {:>2} rows in {elapsed:?}", rows.len());
    }
    println!("largest account: {contacts} contacts, slowest suggestion {slowest_query:?}");
    assert!(
        slowest_query < std::time::Duration::from_millis(100),
        "a keystroke must not cost {slowest_query:?}"
    );
}

/// Build the fixture: accounts, threads, thread messages, an INBOX snapshot,
/// and sent drafts. Returns the number of thread messages written.
fn seed(db: &Database) -> usize {
    let conn = db.conn();
    conn.execute_batch("PRAGMA synchronous=OFF;").unwrap();
    let tx = conn.unchecked_transaction().unwrap();

    let mut total = 0usize;
    for account_index in 0..ACCOUNTS {
        let account_id = format!("acct-{account_index:03}");
        let own = format!("account-{account_index:03}@example.test");
        tx.execute(
            "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
             imap_host, imap_port, encrypted_password)
             VALUES (?1, ?2, ?3, 'example.test', 'smtp.example.test', 587,
                     'imap.example.test', 993, 'x')",
            rusqlite::params![account_id, format!("Account {account_index}"), own],
        )
        .unwrap();

        let messages = if account_index == 0 {
            LARGEST_ACCOUNT_MESSAGES
        } else {
            OTHER_ACCOUNT_MESSAGES
        };

        // Threads hold ~6 messages each, as a real mailbox does.
        let mut thread_id = String::new();
        for message_index in 0..messages {
            if message_index % 6 == 0 {
                thread_id = format!("thread-{account_index:03}-{message_index:06}");
                tx.execute(
                    "INSERT INTO threads (thread_id, subject_normalized, first_seen,
                        last_activity, message_count, account_id)
                     VALUES (?1, ?2, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 0, ?3)",
                    rusqlite::params![thread_id, format!("subject {message_index}"), account_id],
                )
                .unwrap();
            }

            // A few hundred correspondents per account, quoted display names
            // and multi-recipient To lines included.
            let peer = message_index % 400;
            let outbound = message_index % 3 == 0;
            let (from, to) = if outbound {
                (
                    own.clone(),
                    format!(
                        "\"Peer, {peer:03}\" <peer-{peer:03}-{account_index:03}@vendor.test>, \
                         cc-{peer:03}-{account_index:03}@vendor.test"
                    ),
                )
            } else {
                (
                    format!("peer-{peer:03}-{account_index:03}@vendor.test"),
                    format!("{own}, watcher-{account_index:03}@example.test"),
                )
            };

            // Outbound copies carry the Cc/Bcc a scan retains; inbound rows
            // predate that and stay NULL, as an established install's do.
            let cc = outbound.then(|| format!("cc-{peer:03}-{account_index:03}@vendor.test"));
            let bcc = outbound.then(|| format!("bcc-{peer:03}-{account_index:03}@vendor.test"));
            tx.execute(
                "INSERT INTO thread_messages
                    (thread_id, uid, message_id, in_reply_to, reference_ids, folder,
                     from_address, to_addresses, cc_addresses, bcc_addresses, date,
                     subject, is_outbound, snippet)
                 VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?5, ?6, ?9, ?10, ?7, 'Subject', ?8,
                         'snippet')",
                rusqlite::params![
                    thread_id,
                    message_index as i64,
                    format!("<m{account_index}-{message_index}@example.test>"),
                    if outbound { "Sent" } else { "INBOX" },
                    from,
                    to,
                    format!("2026-01-01T00:00:{:02}Z", message_index % 60),
                    i64::from(outbound),
                    cc,
                    bcc,
                ],
            )
            .unwrap();
            total += 1;
        }

        for uid in 0..SUMMARIES_PER_ACCOUNT {
            tx.execute(
                "INSERT INTO indexed_message_summaries
                    (account_id, folder, uidvalidity, uid, message_id, from_addr, to_addr,
                     subject, date, flags_json, size, snippet, thread_id, indexed_at)
                 VALUES (?1, 'INBOX', 1, ?2, ?3, ?4, ?5, 'Subject',
                         'Tue, 12 May 2026 12:00:00 +0000', '[]', 10, 'snippet', NULL,
                         '2026-05-12T12:00:00Z')",
                rusqlite::params![
                    account_id,
                    uid as i64,
                    format!("<s{account_index}-{uid}@example.test>"),
                    format!("Recent Sender {uid} <recent-{uid:02}-{account_index:03}@news.test>"),
                    own,
                ],
            )
            .unwrap();
        }

        for draft in 0..SENT_DRAFTS_PER_ACCOUNT {
            tx.execute(
                "INSERT INTO drafts (id, account_id, status, to_addr, cc_addr, subject,
                    text_content, created_at, updated_at, sent_at)
                 VALUES (?1, ?2, 'sent', ?3, ?4, 'Subject', 'body',
                         '2026-05-01T00:00:00', '2026-05-01T00:00:00', '2026-05-01T00:00:00')",
                rusqlite::params![
                    format!("draft-{account_index:03}-{draft}"),
                    account_id,
                    format!("\"Counsel, {draft}\" <counsel-{draft}-{account_index:03}@law.test>"),
                    format!("paralegal-{draft}-{account_index:03}@law.test"),
                ],
            )
            .unwrap();
        }
    }

    tx.commit().unwrap();
    total
}

fn append_thread_message(
    db: &Database,
    account_id: &str,
    uid: u32,
    from_address: &str,
    to_addresses: &str,
    date: &str,
    is_outbound: bool,
) {
    let thread = db
        .create_thread(&format!("late-{uid}"), date, date, account_id)
        .unwrap();
    db.upsert_thread_message(
        &thread.thread_id,
        uid,
        Some(&format!("<late-{uid}@example.test>")),
        None,
        None,
        if is_outbound { "Sent" } else { "INBOX" },
        from_address,
        to_addresses,
        None,
        None,
        date,
        "Late arrival",
        is_outbound,
        None,
    )
    .unwrap();
}
