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
             WHERE ims.folder = ?1
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
                    COUNT(ims.uid) AS message_count,
                    COALESCE(mis.indexed_at, MAX(ims.indexed_at)) AS indexed_at,
                    CASE
                        WHEN COALESCE(mis.indexed_at, MAX(ims.indexed_at)) IS NULL THEN 'missing'
                        WHEN strftime('%s','now') - strftime('%s', COALESCE(mis.indexed_at, MAX(ims.indexed_at))) <= ?2 THEN 'fresh'
                        WHEN strftime('%s','now') - strftime('%s', COALESCE(mis.indexed_at, MAX(ims.indexed_at))) <= ?3 THEN 'stale'
                        ELSE 'expired'
                    END AS freshness
             FROM accounts a
             LEFT JOIN message_index_state mis
               ON mis.account_id = a.id AND mis.folder = ?1
             LEFT JOIN indexed_message_summaries ims
               ON ims.account_id = a.id AND ims.folder = ?1
             GROUP BY a.id, mis.indexed_at
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

        let missing = rows
            .iter()
            .find(|row| row.account_id == "acct-missing")
            .expect("unrefreshed account should still be present");
        assert_eq!(missing.message_count, 0);
        assert!(missing.indexed_at.is_none());
        assert_eq!(missing.freshness, "missing");
    }
}
