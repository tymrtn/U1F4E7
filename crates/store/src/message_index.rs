// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Local indexed message-summary read model for dashboard first paint.

use crate::db::Database;
use crate::errors::Result;
use crate::models::{
    IndexedMessageInput, IndexedMessageSummary, MessageIndexAccountFreshness, MessageSummary,
};
use rusqlite::params;

/// Parse an envelope date (RFC 2822 as IMAP carries it, or RFC 3339) to unix
/// seconds. Returns None rather than guessing when the string is unreadable.
pub fn parse_date_epoch(raw: Option<&str>) -> Option<i64> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc2822(raw)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(raw))
        .map(|d| d.timestamp())
        .ok()
}

/// Keyset cursor for unified-inbox pagination: the (date_epoch, uid,
/// account_id) of the last row of the previous page, matching the listing's
/// total order exactly.
#[derive(Debug, Clone)]
pub struct UnifiedPageCursor {
    pub date_epoch: Option<i64>,
    pub uid: u32,
    pub account_id: String,
}

const FRESH_AFTER_SECONDS: i64 = 5 * 60;
const STALE_AFTER_SECONDS: i64 = 15 * 60;

impl Database {
    /// Upsert cached message summaries for a mailbox. This is only local state;
    /// callers are responsible for using read-only IMAP fetches to populate it.
    pub fn upsert_indexed_message_summaries(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u64,
        messages: &[IndexedMessageInput],
    ) -> Result<()> {
        let indexed_at = chrono::Utc::now().to_rfc3339();
        self.conn().execute(
            "DELETE FROM indexed_message_summaries WHERE account_id = ?1 AND folder = ?2",
            params![account_id, folder],
        )?;

        for message in messages {
            let flags_json =
                serde_json::to_string(&message.flags).unwrap_or_else(|_| "[]".to_string());
            self.conn().execute(
                "INSERT INTO indexed_message_summaries (
                    account_id, folder, uidvalidity, uid, message_id,
                    from_addr, to_addr, subject, date, flags_json, size,
                    snippet, thread_id, indexed_at, date_epoch
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(account_id, folder, uidvalidity, uid) DO UPDATE SET
                    message_id = excluded.message_id,
                    from_addr = excluded.from_addr,
                    to_addr = excluded.to_addr,
                    subject = excluded.subject,
                    date = excluded.date,
                    flags_json = excluded.flags_json,
                    size = excluded.size,
                    snippet = excluded.snippet,
                    thread_id = excluded.thread_id,
                    indexed_at = excluded.indexed_at,
                    date_epoch = excluded.date_epoch",
                params![
                    account_id,
                    folder,
                    uidvalidity as i64,
                    message.uid as i64,
                    message.message_id,
                    message.from_addr,
                    message.to_addr,
                    message.subject,
                    message.date,
                    flags_json,
                    message.size as i64,
                    message.snippet,
                    message.thread_id,
                    indexed_at,
                    parse_date_epoch(message.date.as_deref()),
                ],
            )?;
        }

        self.conn().execute(
            "INSERT INTO message_index_state (account_id, folder, uidvalidity, indexed_at, last_error)
             VALUES (?1, ?2, ?3, ?4, NULL)
             ON CONFLICT(account_id, folder) DO UPDATE SET
                uidvalidity = excluded.uidvalidity,
                indexed_at = excluded.indexed_at,
                last_error = NULL",
            params![account_id, folder, uidvalidity as i64, indexed_at],
        )?;

        Ok(())
    }

    /// Record that a mailbox index refresh failed. Cached rows are kept for
    /// diagnostics but hidden from unified listing while the error is active.
    ///
    /// The last successful `indexed_at` is preserved: a failed refresh never
    /// overwrites it with the failure time. A mailbox with no prior successful
    /// index keeps a NULL `indexed_at`.
    pub fn record_message_index_error(
        &self,
        account_id: &str,
        folder: &str,
        error: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO message_index_state (account_id, folder, uidvalidity, indexed_at, last_error)
             VALUES (?1, ?2, NULL, NULL, ?3)
             ON CONFLICT(account_id, folder) DO UPDATE SET
                last_error = excluded.last_error",
            params![account_id, folder, error],
        )?;

        Ok(())
    }

    /// List cached summaries across accounts for a folder, newest first.
    /// Compatibility wrapper over the paginated listing (first page).
    pub fn list_indexed_message_summaries(
        &self,
        folder: &str,
        limit: u32,
    ) -> Result<Vec<IndexedMessageSummary>> {
        self.list_indexed_message_summaries_page(folder, limit, None)
    }

    /// One page of cached summaries across accounts, newest first by parsed
    /// date (`date_epoch`), tie-broken by uid then account id so the order is
    /// total and a keyset cursor can continue it exactly. Rows whose account
    /// has an active index error stay hidden, as before.
    pub fn list_indexed_message_summaries_page(
        &self,
        folder: &str,
        limit: u32,
        cursor: Option<&UnifiedPageCursor>,
    ) -> Result<Vec<IndexedMessageSummary>> {
        let has_cursor = i64::from(cursor.is_some());
        let cursor_epoch = cursor
            .map(|c| c.date_epoch.unwrap_or(0))
            .unwrap_or(i64::MAX);
        let cursor_uid = cursor.map(|c| i64::from(c.uid)).unwrap_or(i64::MAX);
        let cursor_account = cursor.map(|c| c.account_id.clone()).unwrap_or_default();
        let mut stmt = self.conn().prepare(
            "SELECT ims.account_id, a.username, a.display_name, ims.folder,
                    ims.uidvalidity, ims.uid, ims.message_id, ims.from_addr,
                    ims.to_addr, ims.subject, ims.date, ims.flags_json, ims.size,
                    ims.snippet, ims.thread_id, ims.indexed_at,
                    CASE
                        WHEN ims.indexed_at IS NULL THEN 'unknown'
                        WHEN strftime('%s','now') - strftime('%s', ims.indexed_at) <= ?2 THEN 'fresh'
                        WHEN strftime('%s','now') - strftime('%s', ims.indexed_at) <= ?3 THEN 'stale'
                        ELSE 'expired'
                    END AS freshness,
                    ims.date_epoch
             FROM indexed_message_summaries ims
             INNER JOIN accounts a ON a.id = ims.account_id
             LEFT JOIN message_index_state mis
               ON mis.account_id = ims.account_id AND mis.folder = ims.folder
             WHERE ims.folder = ?1
               AND mis.last_error IS NULL
               AND (?5 = 0
                    OR (COALESCE(ims.date_epoch, 0), ims.uid, ims.account_id)
                       < (?6, ?7, ?8))
             ORDER BY COALESCE(ims.date_epoch, 0) DESC, ims.uid DESC, ims.account_id DESC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                folder,
                FRESH_AFTER_SECONDS,
                STALE_AFTER_SECONDS,
                limit as i64,
                has_cursor,
                cursor_epoch,
                cursor_uid,
                cursor_account,
            ],
            map_indexed_message_summary,
        )?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    /// Per-account cache metadata for partial/stale dashboard status.
    pub fn list_message_index_account_freshness(
        &self,
        folder: &str,
    ) -> Result<Vec<MessageIndexAccountFreshness>> {
        let mut stmt = self.conn().prepare(
            "SELECT a.id, ?1 AS folder,
                    CASE WHEN mis.last_error IS NOT NULL THEN 0 ELSE COUNT(ims.uid) END AS message_count,
                    COALESCE(mis.indexed_at, MAX(ims.indexed_at)) AS indexed_at,
                    CASE
                        WHEN mis.last_error IS NOT NULL THEN 'unavailable'
                        WHEN COALESCE(mis.indexed_at, MAX(ims.indexed_at)) IS NULL THEN 'missing'
                        WHEN strftime('%s','now') - strftime('%s', COALESCE(mis.indexed_at, MAX(ims.indexed_at))) <= ?2 THEN 'fresh'
                        WHEN strftime('%s','now') - strftime('%s', COALESCE(mis.indexed_at, MAX(ims.indexed_at))) <= ?3 THEN 'stale'
                        ELSE 'expired'
                    END AS freshness,
                    mis.last_error
             FROM accounts a
             LEFT JOIN message_index_state mis
               ON mis.account_id = a.id AND mis.folder = ?1
             LEFT JOIN indexed_message_summaries ims
               ON ims.account_id = a.id AND ims.folder = ?1
             GROUP BY a.id, mis.indexed_at, mis.last_error
             ORDER BY a.created_at",
        )?;
        let rows = stmt.query_map(
            params![folder, FRESH_AFTER_SECONDS, STALE_AFTER_SECONDS],
            |row| {
                Ok(MessageIndexAccountFreshness {
                    account_id: row.get(0)?,
                    folder: row.get(1)?,
                    message_count: row.get::<_, i64>(2)? as usize,
                    indexed_at: row.get(3)?,
                    freshness: row.get(4)?,
                    last_error: row.get(5)?,
                })
            },
        )?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }
}

fn map_indexed_message_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedMessageSummary> {
    let flags_json: String = row.get(11)?;
    let flags = serde_json::from_str::<Vec<String>>(&flags_json).unwrap_or_default();

    Ok(IndexedMessageSummary {
        account_id: row.get(0)?,
        account_username: row.get(1)?,
        account_display_name: row.get(2)?,
        folder: row.get(3)?,
        uidvalidity: row.get::<_, i64>(4)? as u64,
        summary: MessageSummary {
            uid: row.get::<_, i64>(5)? as u32,
            message_id: row.get(6)?,
            from_addr: row.get(7)?,
            to_addr: row.get(8)?,
            subject: row.get(9)?,
            date: row.get(10)?,
            flags,
            size: row.get::<_, i64>(12)? as u32,
            // Not persisted (derived per-fetch from headers); absent on rebuild.
            provider_spam: None,
        },
        snippet: row.get(13)?,
        thread_id: row.get(14)?,
        indexed_at: row.get(15)?,
        freshness: row.get(16)?,
        date_epoch: row.get(17)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(uid: u32, date: &str, subject: &str) -> IndexedMessageInput {
        IndexedMessageInput {
            uid,
            message_id: Some(format!("<{uid}@x>")),
            from_addr: "p@example.test".into(),
            to_addr: "me@example.test".into(),
            subject: subject.into(),
            date: Some(date.into()),
            flags: vec![],
            size: 1,
            snippet: None,
            thread_id: None,
        }
    }

    // ── unified ordering under the SQL cap (sweep blocker #4) ───────────
    // The listing's ORDER BY parsed `ims.date` with strftime, which cannot
    // read the RFC 2822 dates IMAP envelopes carry — every row collapsed to
    // epoch 0 and the cap degenerated to "highest UID wins across accounts".
    // One account's big UIDs then owned the whole page regardless of dates.

    #[test]
    fn cap_keeps_the_newest_message_across_accounts_not_the_biggest_uid() {
        let db = Database::open_memory().unwrap();
        db.test_insert_account_row("acct-big-uids", "big@example.test")
            .unwrap();
        db.test_insert_account_row("acct-small-uids", "small@example.test")
            .unwrap();
        // Older message, huge UID.
        db.upsert_indexed_message_summaries(
            "acct-big-uids",
            "INBOX",
            1,
            &[msg(90_000, "Fri, 01 Aug 2026 10:00:00 +0000", "older")],
        )
        .unwrap();
        // Newer message, tiny UID.
        db.upsert_indexed_message_summaries(
            "acct-small-uids",
            "INBOX",
            1,
            &[msg(5, "Sun, 23 Aug 2026 10:00:00 +0000", "newer")],
        )
        .unwrap();

        let rows = db.list_indexed_message_summaries("INBOX", 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].summary.subject, "newer",
            "the cap must keep the newest by date"
        );
        assert_eq!(rows[0].account_id, "acct-small-uids");
    }

    #[test]
    fn rfc2822_and_rfc3339_dates_order_together() {
        let db = Database::open_memory().unwrap();
        db.test_insert_account_row("acct-a", "a@example.test")
            .unwrap();
        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            1,
            &[
                msg(1, "Fri, 01 Aug 2026 10:00:00 +0000", "aug-1"),
                msg(2, "2026-08-15T10:00:00Z", "aug-15"),
                msg(3, "Sat, 22 Aug 2026 09:00:00 +0000", "aug-22"),
            ],
        )
        .unwrap();
        let rows = db.list_indexed_message_summaries("INBOX", 10).unwrap();
        let subjects: Vec<&str> = rows.iter().map(|r| r.summary.subject.as_str()).collect();
        assert_eq!(subjects, vec!["aug-22", "aug-15", "aug-1"]);
    }

    // ── keyset pagination ───────────────────────────────────────────────

    #[test]
    fn keyset_pagination_returns_the_next_page_without_overlap() {
        let db = Database::open_memory().unwrap();
        db.test_insert_account_row("acct-a", "a@example.test")
            .unwrap();
        db.test_insert_account_row("acct-b", "b@example.test")
            .unwrap();
        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            1,
            &[
                msg(10, "Sun, 23 Aug 2026 10:00:00 +0000", "n1"),
                msg(11, "Fri, 21 Aug 2026 10:00:00 +0000", "n3"),
            ],
        )
        .unwrap();
        db.upsert_indexed_message_summaries(
            "acct-b",
            "INBOX",
            1,
            &[
                msg(3, "Sat, 22 Aug 2026 10:00:00 +0000", "n2"),
                msg(4, "Thu, 20 Aug 2026 10:00:00 +0000", "n4"),
            ],
        )
        .unwrap();

        let page1 = db
            .list_indexed_message_summaries_page("INBOX", 2, None)
            .unwrap();
        let s1: Vec<&str> = page1.iter().map(|r| r.summary.subject.as_str()).collect();
        assert_eq!(s1, vec!["n1", "n2"]);

        let last = page1.last().unwrap();
        let cursor = UnifiedPageCursor {
            date_epoch: last.date_epoch,
            uid: last.summary.uid,
            account_id: last.account_id.clone(),
        };
        let page2 = db
            .list_indexed_message_summaries_page("INBOX", 2, Some(&cursor))
            .unwrap();
        let s2: Vec<&str> = page2.iter().map(|r| r.summary.subject.as_str()).collect();
        assert_eq!(
            s2,
            vec!["n3", "n4"],
            "second page continues without overlap"
        );

        let page3 = db
            .list_indexed_message_summaries_page(
                "INBOX",
                2,
                Some(&UnifiedPageCursor {
                    date_epoch: page2.last().unwrap().date_epoch,
                    uid: page2.last().unwrap().summary.uid,
                    account_id: page2.last().unwrap().account_id.clone(),
                }),
            )
            .unwrap();
        assert!(page3.is_empty(), "no third page");
    }

    #[test]
    fn refreshed_empty_mailbox_reports_freshness_from_index_state() {
        let db = Database::open_memory().unwrap();
        db.test_insert_account_row("acct-empty", "empty@example.test")
            .unwrap();
        db.test_insert_account_row("acct-missing", "missing@example.test")
            .unwrap();

        db.upsert_indexed_message_summaries("acct-empty", "INBOX", 123, &[])
            .unwrap();

        let rows = db.list_message_index_account_freshness("INBOX").unwrap();
        let empty = rows
            .iter()
            .find(|row| row.account_id == "acct-empty")
            .expect("refreshed empty account should be present");

        assert_eq!(empty.folder, "INBOX");
        assert_eq!(empty.message_count, 0);
        assert!(empty.indexed_at.is_some());
        assert_eq!(empty.freshness, "fresh");
        assert!(empty.last_error.is_none());

        let missing = rows
            .iter()
            .find(|row| row.account_id == "acct-missing")
            .expect("unrefreshed account should still be present");
        assert_eq!(missing.message_count, 0);
        assert!(missing.indexed_at.is_none());
        assert_eq!(missing.freshness, "missing");
        assert!(missing.last_error.is_none());
    }

    #[test]
    fn failed_refresh_marks_cached_mailbox_unavailable_and_hides_rows() {
        let db = Database::open_memory().unwrap();
        db.test_insert_account_row("acct-failed", "failed@example.test")
            .unwrap();

        db.upsert_indexed_message_summaries(
            "acct-failed",
            "INBOX",
            123,
            &[IndexedMessageInput {
                uid: 99,
                message_id: Some("<phantom@example.test>".to_string()),
                from_addr: "Court Notifications <notice@example.test>".to_string(),
                to_addr: "failed@example.test".to_string(),
                subject: "Victim impact statement filing".to_string(),
                date: Some("Thu, 09 Jul 2026 08:42:00 +0000".to_string()),
                flags: vec![],
                size: 42,
                snippet: Some("The clerk has received the victim impact statement".to_string()),
                thread_id: None,
            }],
        )
        .unwrap();

        let indexed_at_before = db
            .list_message_index_account_freshness("INBOX")
            .unwrap()
            .into_iter()
            .find(|row| row.account_id == "acct-failed")
            .and_then(|row| row.indexed_at);
        assert!(indexed_at_before.is_some());

        db.record_message_index_error("acct-failed", "INBOX", "IMAP: auth failed")
            .unwrap();

        assert!(
            db.list_indexed_message_summaries("INBOX", 10)
                .unwrap()
                .is_empty()
        );

        let freshness = db.list_message_index_account_freshness("INBOX").unwrap();
        let failed = freshness
            .iter()
            .find(|row| row.account_id == "acct-failed")
            .expect("failed account freshness row");
        assert_eq!(failed.message_count, 0);
        assert_eq!(failed.freshness, "unavailable");
        assert_eq!(failed.last_error.as_deref(), Some("IMAP: auth failed"));
        // The last successful index time is preserved, not overwritten with the
        // failure time.
        assert_eq!(failed.indexed_at, indexed_at_before);
    }

    #[test]
    fn record_error_without_prior_index_keeps_indexed_at_null() {
        let db = Database::open_memory().unwrap();
        db.test_insert_account_row("acct-never", "never@example.test")
            .unwrap();

        db.record_message_index_error("acct-never", "INBOX", "IMAP: auth failed")
            .unwrap();

        let freshness = db.list_message_index_account_freshness("INBOX").unwrap();
        let never = freshness
            .iter()
            .find(|row| row.account_id == "acct-never")
            .expect("never-indexed account freshness row");
        assert!(never.indexed_at.is_none());
        assert_eq!(never.message_count, 0);
        assert_eq!(never.freshness, "unavailable");
        assert_eq!(never.last_error.as_deref(), Some("IMAP: auth failed"));
    }

    #[test]
    fn successful_refresh_clears_prior_error_and_restores_rows() {
        let db = Database::open_memory().unwrap();
        db.test_insert_account_row("acct-recover", "recover@example.test")
            .unwrap();

        let court_row = |uid: u32| IndexedMessageInput {
            uid,
            message_id: Some(format!("<court-{uid}@example.test>")),
            from_addr: "Court Notifications <notice@example.test>".to_string(),
            to_addr: "recover@example.test".to_string(),
            subject: "Victim impact statement filing".to_string(),
            date: Some("Thu, 09 Jul 2026 08:42:00 +0000".to_string()),
            flags: vec![],
            size: 42,
            snippet: Some("The clerk has received the victim impact statement".to_string()),
            thread_id: None,
        };

        // Successful cached index.
        db.upsert_indexed_message_summaries("acct-recover", "INBOX", 123, &[court_row(99)])
            .unwrap();

        // Refresh error hides the row and marks the account unavailable.
        db.record_message_index_error("acct-recover", "INBOX", "fetch INBOX: timeout")
            .unwrap();
        assert!(
            db.list_indexed_message_summaries("INBOX", 10)
                .unwrap()
                .is_empty()
        );

        // A later successful refresh clears the error and restores visibility.
        db.upsert_indexed_message_summaries("acct-recover", "INBOX", 123, &[court_row(100)])
            .unwrap();

        let rows = db.list_indexed_message_summaries("INBOX", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary.uid, 100);

        let freshness = db.list_message_index_account_freshness("INBOX").unwrap();
        let recovered = freshness
            .iter()
            .find(|row| row.account_id == "acct-recover")
            .expect("recovered account freshness row");
        assert!(recovered.last_error.is_none());
        assert_eq!(recovered.freshness, "fresh");
        assert_eq!(recovered.message_count, 1);
        assert!(recovered.indexed_at.is_some());
    }
}
