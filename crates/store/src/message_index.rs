// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Local indexed message-summary read model for dashboard first paint.

use crate::db::Database;
use crate::errors::Result;
use crate::models::{
    IndexedMessageInput, IndexedMessageSummary, MessageIndexAccountFreshness, MessageSummary,
};
use rusqlite::params;

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
                    snippet, thread_id, indexed_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
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
                    indexed_at = excluded.indexed_at",
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
    pub fn list_indexed_message_summaries(
        &self,
        folder: &str,
        limit: u32,
    ) -> Result<Vec<IndexedMessageSummary>> {
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
                    END AS freshness
             FROM indexed_message_summaries ims
             INNER JOIN accounts a ON a.id = ims.account_id
             LEFT JOIN message_index_state mis
               ON mis.account_id = ims.account_id AND mis.folder = ims.folder
             WHERE ims.folder = ?1
               AND mis.last_error IS NULL
             ORDER BY COALESCE(strftime('%s', ims.date), 0) DESC, ims.uid DESC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                folder,
                FRESH_AFTER_SECONDS,
                STALE_AFTER_SECONDS,
                limit as i64
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
        },
        snippet: row.get(13)?,
        thread_id: row.get(14)?,
        indexed_at: row.get(15)?,
        freshness: row.get(16)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
