// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::{Thread, ThreadMessage};
use rusqlite::params;
use uuid::Uuid;

/// The full SELECT column list for threads.
const THREAD_COLS: &str =
    "thread_id, subject_normalized, first_seen, last_activity, message_count, account_id";

/// The full SELECT column list for thread_messages.
const THREAD_MSG_COLS: &str = "id, thread_id, uid, message_id, in_reply_to, reference_ids, folder, \
     from_address, to_addresses, date, subject, is_outbound, snippet";

impl Database {
    // ── Thread CRUD ──────────────────────────────────────────────────

    /// Create a new thread.
    pub fn create_thread(
        &self,
        subject_normalized: &str,
        first_seen: &str,
        last_activity: &str,
        account_id: &str,
    ) -> Result<Thread> {
        let thread_id = Uuid::new_v4().to_string();

        self.conn().execute(
            "INSERT INTO threads (thread_id, subject_normalized, first_seen, last_activity, message_count, account_id)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            params![thread_id, subject_normalized, first_seen, last_activity, account_id],
        )?;

        self.get_thread(&thread_id)?.ok_or_else(|| {
            crate::errors::StoreError::Config(format!("thread not found after insert: {thread_id}"))
        })
    }

    /// Get a thread by ID.
    pub fn get_thread(&self, thread_id: &str) -> Result<Option<Thread>> {
        let sql = format!("SELECT {THREAD_COLS} FROM threads WHERE thread_id = ?1");
        let mut stmt = self.conn().prepare(&sql)?;
        let thread = stmt
            .query_row(params![thread_id], Self::map_thread)
            .optional()?;
        Ok(thread)
    }

    /// Find a thread by normalized subject and account.
    pub fn find_thread_by_subject(
        &self,
        subject_normalized: &str,
        account_id: &str,
    ) -> Result<Option<Thread>> {
        let sql = format!(
            "SELECT {THREAD_COLS} FROM threads \
             WHERE subject_normalized = ?1 AND account_id = ?2"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let thread = stmt
            .query_row(params![subject_normalized, account_id], Self::map_thread)
            .optional()?;
        Ok(thread)
    }

    /// List threads, optionally filtered by account, ordered by last_activity desc.
    pub fn list_threads(&self, account_id: Option<&str>, limit: u32) -> Result<Vec<Thread>> {
        let threads = match account_id {
            Some(acct) => {
                let sql = format!(
                    "SELECT {THREAD_COLS} FROM threads \
                     WHERE account_id = ?1 ORDER BY last_activity DESC LIMIT ?2"
                );
                let mut stmt = self.conn().prepare(&sql)?;
                let rows = stmt.query_map(params![acct, limit], Self::map_thread)?;
                rows.filter_map(|r| r.ok()).collect()
            }
            None => {
                let sql = format!(
                    "SELECT {THREAD_COLS} FROM threads \
                     ORDER BY last_activity DESC LIMIT ?1"
                );
                let mut stmt = self.conn().prepare(&sql)?;
                let rows = stmt.query_map(params![limit], Self::map_thread)?;
                rows.filter_map(|r| r.ok()).collect()
            }
        };
        Ok(threads)
    }

    /// Update thread timestamps and message count from its messages.
    pub fn refresh_thread_stats(&self, thread_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE threads SET
                message_count = (SELECT COUNT(*) FROM thread_messages WHERE thread_id = ?1),
                first_seen = COALESCE((SELECT MIN(date) FROM thread_messages WHERE thread_id = ?1), first_seen),
                last_activity = COALESCE((SELECT MAX(date) FROM thread_messages WHERE thread_id = ?1), last_activity)
             WHERE thread_id = ?1",
            params![thread_id],
        )?;
        Ok(())
    }

    /// Delete a thread and all its messages.
    pub fn delete_thread(&self, thread_id: &str) -> Result<bool> {
        self.conn().execute(
            "DELETE FROM thread_messages WHERE thread_id = ?1",
            params![thread_id],
        )?;
        let rows = self.conn().execute(
            "DELETE FROM threads WHERE thread_id = ?1",
            params![thread_id],
        )?;
        Ok(rows > 0)
    }

    // ── Thread Message CRUD ──────────────────────────────────────────

    /// Insert or update a thread message (upsert by message_id + folder).
    /// Returns the thread_message id.
    pub fn upsert_thread_message(
        &self,
        thread_id: &str,
        uid: u32,
        message_id: Option<&str>,
        in_reply_to: Option<&str>,
        references: Option<&str>,
        folder: &str,
        from_address: &str,
        to_addresses: &str,
        date: &str,
        subject: &str,
        is_outbound: bool,
        snippet: Option<&str>,
    ) -> Result<i64> {
        // Check if we already have this message (by message_id + folder, or uid + folder)
        let existing_id: Option<i64> = if let Some(mid) = message_id {
            let id: Option<i64> = self
                .conn()
                .query_row(
                    "SELECT id FROM thread_messages WHERE message_id = ?1 AND folder = ?2",
                    params![mid, folder],
                    |row| row.get(0),
                )
                .optional()?;
            id
        } else {
            let id: Option<i64> = self.conn().query_row(
                "SELECT id FROM thread_messages WHERE uid = ?1 AND folder = ?2 AND thread_id = ?3",
                params![uid as i64, folder, thread_id],
                |row| row.get(0),
            ).optional()?;
            id
        };

        let is_outbound_int: i32 = if is_outbound { 1 } else { 0 };

        if let Some(id) = existing_id {
            // Update existing
            self.conn().execute(
                "UPDATE thread_messages SET
                    thread_id = ?1, uid = ?2, in_reply_to = ?3, reference_ids = ?4,
                    from_address = ?5, to_addresses = ?6, date = ?7, subject = ?8,
                    is_outbound = ?9, snippet = ?10
                 WHERE id = ?11",
                params![
                    thread_id,
                    uid as i64,
                    in_reply_to,
                    references,
                    from_address,
                    to_addresses,
                    date,
                    subject,
                    is_outbound_int,
                    snippet,
                    id
                ],
            )?;
            Ok(id)
        } else {
            // Insert new
            self.conn().execute(
                "INSERT INTO thread_messages
                    (thread_id, uid, message_id, in_reply_to, reference_ids, folder,
                     from_address, to_addresses, date, subject, is_outbound, snippet)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    thread_id,
                    uid as i64,
                    message_id,
                    in_reply_to,
                    references,
                    folder,
                    from_address,
                    to_addresses,
                    date,
                    subject,
                    is_outbound_int,
                    snippet
                ],
            )?;
            Ok(self.conn().last_insert_rowid())
        }
    }

    /// Get all messages in a thread, ordered by date ascending.
    pub fn get_thread_messages(&self, thread_id: &str) -> Result<Vec<ThreadMessage>> {
        let sql = format!(
            "SELECT {THREAD_MSG_COLS} FROM thread_messages \
             WHERE thread_id = ?1 ORDER BY date ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![thread_id], Self::map_thread_message)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Find which thread a message belongs to, by its Message-ID header.
    /// Scoped to a specific account to prevent cross-account thread leakage.
    pub fn find_thread_by_message_id(
        &self,
        message_id: &str,
        account_id: &str,
    ) -> Result<Option<String>> {
        let thread_id: Option<String> = self
            .conn()
            .query_row(
                "SELECT tm.thread_id FROM thread_messages tm \
                 INNER JOIN threads t ON t.thread_id = tm.thread_id \
                 WHERE tm.message_id = ?1 AND t.account_id = ?2 LIMIT 1",
                params![message_id, account_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(thread_id)
    }

    /// Find which thread a message belongs to, by UID and folder.
    pub fn find_thread_by_uid(&self, uid: u32, folder: &str) -> Result<Option<String>> {
        let thread_id: Option<String> = self
            .conn()
            .query_row(
                "SELECT thread_id FROM thread_messages WHERE uid = ?1 AND folder = ?2 LIMIT 1",
                params![uid as i64, folder],
                |row| row.get(0),
            )
            .optional()?;
        Ok(thread_id)
    }

    /// Find a thread by any referenced Message-ID (In-Reply-To or References).
    /// Returns the thread_id if any message in our DB has a matching message_id.
    /// Scoped to a specific account to prevent cross-account thread leakage.
    pub fn find_thread_by_references(
        &self,
        references: &[&str],
        account_id: &str,
    ) -> Result<Option<String>> {
        if references.is_empty() {
            return Ok(None);
        }
        // Build a placeholder list: (?1, ?2, ..., ?N) for references,
        // then ?N+1 for account_id
        let placeholders: Vec<String> = (1..=references.len()).map(|i| format!("?{i}")).collect();
        let account_param_idx = references.len() + 1;
        let sql = format!(
            "SELECT tm.thread_id FROM thread_messages tm \
             INNER JOIN threads t ON t.thread_id = tm.thread_id \
             WHERE tm.message_id IN ({}) AND t.account_id = ?{account_param_idx} LIMIT 1",
            placeholders.join(", ")
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::types::ToSql> = references
            .iter()
            .map(|r| r as &dyn rusqlite::types::ToSql)
            .collect();
        params.push(&account_id as &dyn rusqlite::types::ToSql);
        let thread_id: Option<String> = stmt
            .query_row(params.as_slice(), |row| row.get(0))
            .optional()?;
        Ok(thread_id)
    }

    /// Get thread context for a message (for enriching inbox/read output).
    /// Returns (thread_id, message_count, has_outbound_reply, reply_uid).
    pub fn get_thread_context_for_uid(
        &self,
        uid: u32,
        folder: &str,
    ) -> Result<Option<ThreadContext>> {
        let thread_id = match self.find_thread_by_uid(uid, folder)? {
            Some(tid) => tid,
            None => return Ok(None),
        };

        let thread = match self.get_thread(&thread_id)? {
            Some(t) => t,
            None => return Ok(None),
        };

        // Check if there's an outbound reply in this thread
        let reply_info: Option<(i64, String)> = self
            .conn()
            .query_row(
                "SELECT uid, folder FROM thread_messages \
             WHERE thread_id = ?1 AND is_outbound = 1 \
             ORDER BY date DESC LIMIT 1",
                params![thread_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;

        let (has_reply, reply_uid, reply_folder) = match reply_info {
            Some((uid, folder)) => (true, Some(uid as u32), Some(folder)),
            None => (false, None, None),
        };

        Ok(Some(ThreadContext {
            thread_id,
            thread_count: thread.message_count as u32,
            has_reply,
            reply_uid,
            reply_folder,
        }))
    }

    /// Get the last-synced UID for a folder/account pair (for incremental sync).
    pub fn get_last_synced_uid(&self, account_id: &str, folder: &str) -> Result<Option<u32>> {
        let uid: Option<i64> = self
            .conn()
            .query_row(
                "SELECT last_uid FROM thread_sync_state WHERE account_id = ?1 AND folder = ?2",
                params![account_id, folder],
                |row| row.get(0),
            )
            .optional()?;
        Ok(uid.map(|u| u as u32))
    }

    /// Set the last-synced UID for a folder/account pair.
    pub fn set_last_synced_uid(&self, account_id: &str, folder: &str, uid: u32) -> Result<()> {
        self.conn().execute(
            "INSERT INTO thread_sync_state (account_id, folder, last_uid, synced_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(account_id, folder) DO UPDATE SET
                last_uid = excluded.last_uid,
                synced_at = excluded.synced_at",
            params![account_id, folder, uid as i64],
        )?;
        Ok(())
    }

    /// Get the stored UIDVALIDITY for a folder/account pair.
    pub fn get_uidvalidity(&self, account_id: &str, folder: &str) -> Result<Option<u32>> {
        let val: Option<i64> = self
            .conn()
            .query_row(
                "SELECT uidvalidity FROM thread_sync_state WHERE account_id = ?1 AND folder = ?2",
                params![account_id, folder],
                |row| row.get(0),
            )
            .optional()?;
        Ok(val.map(|v| v as u32))
    }

    /// Set the UIDVALIDITY for a folder/account pair.
    pub fn set_uidvalidity(&self, account_id: &str, folder: &str, uidvalidity: u32) -> Result<()> {
        // If a sync state row exists, update it; otherwise create one
        let rows = self.conn().execute(
            "UPDATE thread_sync_state SET uidvalidity = ?3 \
             WHERE account_id = ?1 AND folder = ?2",
            params![account_id, folder, uidvalidity as i64],
        )?;
        if rows == 0 {
            self.conn().execute(
                "INSERT INTO thread_sync_state (account_id, folder, last_uid, uidvalidity, synced_at)
                 VALUES (?1, ?2, 0, ?3, datetime('now'))",
                params![account_id, folder, uidvalidity as i64],
            )?;
        }
        Ok(())
    }

    /// Reset sync state for a folder when UIDVALIDITY changes.
    /// Deletes all thread_messages for that (account, folder) and resets last_uid to 0.
    pub fn reset_folder_sync(
        &self,
        account_id: &str,
        folder: &str,
        new_uidvalidity: u32,
    ) -> Result<u32> {
        // Delete thread_messages for this account+folder
        // We need to join through threads to scope by account_id
        let deleted = self.conn().execute(
            "DELETE FROM thread_messages WHERE folder = ?1 AND thread_id IN \
             (SELECT thread_id FROM threads WHERE account_id = ?2)",
            params![folder, account_id],
        )?;

        // Clean up any threads that now have zero messages
        self.conn().execute(
            "DELETE FROM threads WHERE account_id = ?1 AND thread_id NOT IN \
             (SELECT DISTINCT thread_id FROM thread_messages)",
            params![account_id],
        )?;

        // Reset the sync state with new uidvalidity
        self.conn().execute(
            "INSERT INTO thread_sync_state (account_id, folder, last_uid, uidvalidity, synced_at)
             VALUES (?1, ?2, 0, ?3, datetime('now'))
             ON CONFLICT(account_id, folder) DO UPDATE SET
                last_uid = 0,
                uidvalidity = excluded.uidvalidity,
                synced_at = excluded.synced_at",
            params![account_id, folder, new_uidvalidity as i64],
        )?;

        Ok(deleted as u32)
    }

    // NOTE: get_sent_folder / get_drafts_folder / set_detected_folder /
    // get_detected_folders used to live here but were moved to db.rs after
    // this file was orphaned. Do not re-add them — the canonical
    // implementations are in db.rs.

    // ── Row mappers ──────────────────────────────────────────────────

    fn map_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
        Ok(Thread {
            thread_id: row.get(0)?,
            subject_normalized: row.get(1)?,
            first_seen: row.get(2)?,
            last_activity: row.get(3)?,
            message_count: row.get(4)?,
            account_id: row.get(5)?,
        })
    }

    fn map_thread_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadMessage> {
        let uid_i64: i64 = row.get(2)?;
        let is_outbound_int: i32 = row.get(11)?;
        Ok(ThreadMessage {
            id: row.get(0)?,
            thread_id: row.get(1)?,
            uid: uid_i64 as u32,
            message_id: row.get(3)?,
            in_reply_to: row.get(4)?,
            references: row.get(5)?,
            folder: row.get(6)?,
            from_address: row.get(7)?,
            to_addresses: row.get(8)?,
            date: row.get(9)?,
            subject: row.get(10)?,
            is_outbound: is_outbound_int != 0,
            snippet: row.get(12)?,
        })
    }
}

/// Thread context for enriching inbox/read output.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThreadContext {
    pub thread_id: String,
    pub thread_count: u32,
    pub has_reply: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_uid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_folder: Option<String>,
}

/// Extension trait for optional rusqlite query results.
trait OptionalExt<T> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for std::result::Result<T, rusqlite::Error> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(val) => Ok(Some(val)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Database;

    #[test]
    fn create_and_list_threads() {
        let db = Database::open_memory().unwrap();

        let thread = db
            .create_thread(
                "test subject",
                "2026-03-28T10:00:00",
                "2026-03-28T10:00:00",
                "test-account-id",
            )
            .unwrap();

        assert_eq!(thread.subject_normalized, "test subject");
        assert_eq!(thread.message_count, 0);
        assert_eq!(thread.account_id, "test-account-id");

        let threads = db.list_threads(Some("test-account-id"), 50).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, thread.thread_id);
    }

    #[test]
    fn upsert_and_get_messages() {
        let db = Database::open_memory().unwrap();

        let thread = db
            .create_thread(
                "test",
                "2026-03-28T10:00:00",
                "2026-03-28T12:00:00",
                "acct1",
            )
            .unwrap();

        // Insert first message
        db.upsert_thread_message(
            &thread.thread_id,
            100,
            Some("<msg1@example.com>"),
            None,
            None,
            "INBOX",
            "alice@example.com",
            "bob@example.com",
            "2026-03-28T10:00:00",
            "Test Subject",
            false,
            Some("Hello, this is a test message..."),
        )
        .unwrap();

        // Insert reply
        db.upsert_thread_message(
            &thread.thread_id,
            50,
            Some("<msg2@example.com>"),
            Some("<msg1@example.com>"),
            Some("<msg1@example.com>"),
            "Sent",
            "bob@example.com",
            "alice@example.com",
            "2026-03-28T12:00:00",
            "Re: Test Subject",
            true,
            Some("Thanks for your message..."),
        )
        .unwrap();

        db.refresh_thread_stats(&thread.thread_id).unwrap();

        let thread = db.get_thread(&thread.thread_id).unwrap().unwrap();
        assert_eq!(thread.message_count, 2);

        let messages = db.get_thread_messages(&thread.thread_id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0].from_address.as_deref(),
            Some("alice@example.com")
        );
        assert!(!messages[0].is_outbound);
        assert_eq!(messages[1].from_address.as_deref(), Some("bob@example.com"));
        assert!(messages[1].is_outbound);
    }

    #[test]
    fn find_thread_by_message_id() {
        let db = Database::open_memory().unwrap();

        let thread = db
            .create_thread(
                "test",
                "2026-03-28T10:00:00",
                "2026-03-28T10:00:00",
                "acct1",
            )
            .unwrap();

        db.upsert_thread_message(
            &thread.thread_id,
            100,
            Some("<unique@example.com>"),
            None,
            None,
            "INBOX",
            "a@b.com",
            "c@d.com",
            "2026-03-28T10:00:00",
            "Test",
            false,
            None,
        )
        .unwrap();

        let found = db
            .find_thread_by_message_id("<unique@example.com>", "acct1")
            .unwrap();
        assert_eq!(found, Some(thread.thread_id.clone()));

        let not_found = db
            .find_thread_by_message_id("<nonexistent@example.com>", "acct1")
            .unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn find_thread_by_references() {
        let db = Database::open_memory().unwrap();

        let thread = db
            .create_thread(
                "test",
                "2026-03-28T10:00:00",
                "2026-03-28T10:00:00",
                "acct1",
            )
            .unwrap();

        db.upsert_thread_message(
            &thread.thread_id,
            100,
            Some("<original@example.com>"),
            None,
            None,
            "INBOX",
            "a@b.com",
            "c@d.com",
            "2026-03-28T10:00:00",
            "Test",
            false,
            None,
        )
        .unwrap();

        // Search by references that include the original message-id
        let found = db
            .find_thread_by_references(&["<nonexist@x.com>", "<original@example.com>"], "acct1")
            .unwrap();
        assert_eq!(found, Some(thread.thread_id.clone()));

        // No match
        let not_found = db
            .find_thread_by_references(&["<nope@x.com>"], "acct1")
            .unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn thread_context_for_uid() {
        let db = Database::open_memory().unwrap();

        let thread = db
            .create_thread(
                "test",
                "2026-03-28T10:00:00",
                "2026-03-28T12:00:00",
                "acct1",
            )
            .unwrap();

        db.upsert_thread_message(
            &thread.thread_id,
            100,
            Some("<msg1@x.com>"),
            None,
            None,
            "INBOX",
            "them@x.com",
            "me@x.com",
            "2026-03-28T10:00:00",
            "Test",
            false,
            None,
        )
        .unwrap();
        db.upsert_thread_message(
            &thread.thread_id,
            50,
            Some("<msg2@x.com>"),
            Some("<msg1@x.com>"),
            None,
            "Sent",
            "me@x.com",
            "them@x.com",
            "2026-03-28T12:00:00",
            "Re: Test",
            true,
            None,
        )
        .unwrap();
        db.refresh_thread_stats(&thread.thread_id).unwrap();

        let ctx = db
            .get_thread_context_for_uid(100, "INBOX")
            .unwrap()
            .unwrap();
        assert_eq!(ctx.thread_count, 2);
        assert!(ctx.has_reply);
        assert_eq!(ctx.reply_uid, Some(50));
        assert_eq!(ctx.reply_folder.as_deref(), Some("Sent"));
    }

    #[test]
    fn sync_state() {
        let db = Database::open_memory().unwrap();

        assert!(db.get_last_synced_uid("acct1", "INBOX").unwrap().is_none());

        db.set_last_synced_uid("acct1", "INBOX", 500).unwrap();
        assert_eq!(db.get_last_synced_uid("acct1", "INBOX").unwrap(), Some(500));

        // Update
        db.set_last_synced_uid("acct1", "INBOX", 600).unwrap();
        assert_eq!(db.get_last_synced_uid("acct1", "INBOX").unwrap(), Some(600));
    }

    #[test]
    fn detected_folders() {
        let db = Database::open_memory().unwrap();

        assert!(db.get_sent_folder("acct1").unwrap().is_none());

        db.set_detected_folder("acct1", "sent", "Sent Messages")
            .unwrap();
        assert_eq!(
            db.get_sent_folder("acct1").unwrap().as_deref(),
            Some("Sent Messages")
        );

        db.set_detected_folder("acct1", "drafts", "Drafts").unwrap();
        let folders = db.get_detected_folders("acct1").unwrap();
        assert_eq!(folders.len(), 2);
    }

    #[test]
    fn detected_drafts_folder() {
        let db = Database::open_memory().unwrap();

        // No drafts folder cached yet
        assert!(db.get_drafts_folder("acct1").unwrap().is_none());

        // Cache a standard drafts folder
        db.set_detected_folder("acct1", "drafts", "Drafts").unwrap();
        assert_eq!(
            db.get_drafts_folder("acct1").unwrap().as_deref(),
            Some("Drafts")
        );

        // Gmail-style drafts folder overwrites the cached value
        db.set_detected_folder("acct1", "drafts", "[Gmail]/Drafts")
            .unwrap();
        assert_eq!(
            db.get_drafts_folder("acct1").unwrap().as_deref(),
            Some("[Gmail]/Drafts")
        );

        // Different accounts have independent caches
        assert!(db.get_drafts_folder("acct2").unwrap().is_none());
        db.set_detected_folder("acct2", "drafts", "INBOX.Drafts")
            .unwrap();
        assert_eq!(
            db.get_drafts_folder("acct2").unwrap().as_deref(),
            Some("INBOX.Drafts")
        );
        // acct1 still has Gmail drafts
        assert_eq!(
            db.get_drafts_folder("acct1").unwrap().as_deref(),
            Some("[Gmail]/Drafts")
        );
    }

    #[test]
    fn upsert_dedup_by_message_id() {
        let db = Database::open_memory().unwrap();

        let thread = db
            .create_thread(
                "test",
                "2026-03-28T10:00:00",
                "2026-03-28T10:00:00",
                "acct1",
            )
            .unwrap();

        let id1 = db
            .upsert_thread_message(
                &thread.thread_id,
                100,
                Some("<msg@x.com>"),
                None,
                None,
                "INBOX",
                "a@b.com",
                "c@d.com",
                "2026-03-28T10:00:00",
                "Test",
                false,
                Some("v1"),
            )
            .unwrap();

        // Upsert same message — should update, not duplicate
        let id2 = db
            .upsert_thread_message(
                &thread.thread_id,
                100,
                Some("<msg@x.com>"),
                None,
                None,
                "INBOX",
                "a@b.com",
                "c@d.com",
                "2026-03-28T10:00:00",
                "Test",
                false,
                Some("v2"),
            )
            .unwrap();

        assert_eq!(id1, id2);

        db.refresh_thread_stats(&thread.thread_id).unwrap();
        let thread = db.get_thread(&thread.thread_id).unwrap().unwrap();
        assert_eq!(thread.message_count, 1);
    }

    #[test]
    fn delete_thread() {
        let db = Database::open_memory().unwrap();

        let thread = db
            .create_thread(
                "test",
                "2026-03-28T10:00:00",
                "2026-03-28T10:00:00",
                "acct1",
            )
            .unwrap();

        db.upsert_thread_message(
            &thread.thread_id,
            100,
            Some("<msg@x.com>"),
            None,
            None,
            "INBOX",
            "a@b.com",
            "c@d.com",
            "2026-03-28T10:00:00",
            "Test",
            false,
            None,
        )
        .unwrap();

        assert!(db.delete_thread(&thread.thread_id).unwrap());
        assert!(db.get_thread(&thread.thread_id).unwrap().is_none());
        assert!(
            db.get_thread_messages(&thread.thread_id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cross_account_thread_isolation() {
        let db = Database::open_memory().unwrap();

        // Create threads for two different accounts
        let thread_a = db
            .create_thread(
                "shared subject",
                "2026-03-28T10:00:00",
                "2026-03-28T10:00:00",
                "account-alice",
            )
            .unwrap();
        let thread_b = db
            .create_thread(
                "shared subject",
                "2026-03-28T10:00:00",
                "2026-03-28T10:00:00",
                "account-bob",
            )
            .unwrap();

        // Add a message with the same Message-ID to account A's thread
        db.upsert_thread_message(
            &thread_a.thread_id,
            100,
            Some("<shared-msg@example.com>"),
            None,
            None,
            "INBOX",
            "sender@example.com",
            "alice@example.com",
            "2026-03-28T10:00:00",
            "Shared Subject",
            false,
            None,
        )
        .unwrap();

        // Add a different message to account B's thread
        db.upsert_thread_message(
            &thread_b.thread_id,
            200,
            Some("<bob-msg@example.com>"),
            None,
            None,
            "INBOX",
            "sender@example.com",
            "bob@example.com",
            "2026-03-28T10:00:00",
            "Shared Subject",
            false,
            None,
        )
        .unwrap();

        // find_thread_by_message_id should only find in the correct account
        let found_alice = db
            .find_thread_by_message_id("<shared-msg@example.com>", "account-alice")
            .unwrap();
        assert_eq!(found_alice, Some(thread_a.thread_id.clone()));

        let found_bob = db
            .find_thread_by_message_id("<shared-msg@example.com>", "account-bob")
            .unwrap();
        assert!(
            found_bob.is_none(),
            "should not find alice's message in bob's account"
        );

        // find_thread_by_references should respect account scoping
        let found_refs = db
            .find_thread_by_references(&["<shared-msg@example.com>"], "account-bob")
            .unwrap();
        assert!(
            found_refs.is_none(),
            "should not cross account boundary via references"
        );

        let found_refs_alice = db
            .find_thread_by_references(&["<shared-msg@example.com>"], "account-alice")
            .unwrap();
        assert_eq!(found_refs_alice, Some(thread_a.thread_id));
    }

    #[test]
    fn uidvalidity_tracking() {
        let db = Database::open_memory().unwrap();

        // Initially no UIDVALIDITY stored
        assert!(db.get_uidvalidity("acct1", "INBOX").unwrap().is_none());

        // Store UIDVALIDITY
        db.set_uidvalidity("acct1", "INBOX", 12345).unwrap();
        assert_eq!(db.get_uidvalidity("acct1", "INBOX").unwrap(), Some(12345));

        // Update UIDVALIDITY
        db.set_uidvalidity("acct1", "INBOX", 99999).unwrap();
        assert_eq!(db.get_uidvalidity("acct1", "INBOX").unwrap(), Some(99999));
    }

    #[test]
    fn uidvalidity_reset_clears_messages() {
        let db = Database::open_memory().unwrap();

        // Create a thread with messages
        let thread = db
            .create_thread(
                "test subject",
                "2026-03-28T10:00:00",
                "2026-03-28T12:00:00",
                "acct1",
            )
            .unwrap();

        db.upsert_thread_message(
            &thread.thread_id,
            100,
            Some("<msg1@x.com>"),
            None,
            None,
            "INBOX",
            "a@b.com",
            "c@d.com",
            "2026-03-28T10:00:00",
            "Test",
            false,
            None,
        )
        .unwrap();
        db.upsert_thread_message(
            &thread.thread_id,
            200,
            Some("<msg2@x.com>"),
            None,
            None,
            "INBOX",
            "c@d.com",
            "a@b.com",
            "2026-03-28T12:00:00",
            "Re: Test",
            true,
            None,
        )
        .unwrap();
        db.refresh_thread_stats(&thread.thread_id).unwrap();

        // Set initial sync state
        db.set_last_synced_uid("acct1", "INBOX", 200).unwrap();
        db.set_uidvalidity("acct1", "INBOX", 1000).unwrap();

        // Verify initial state
        assert_eq!(db.get_thread_messages(&thread.thread_id).unwrap().len(), 2);
        assert_eq!(db.get_last_synced_uid("acct1", "INBOX").unwrap(), Some(200));

        // Reset folder sync (simulating UIDVALIDITY change)
        let deleted = db.reset_folder_sync("acct1", "INBOX", 2000).unwrap();
        assert_eq!(deleted, 2);

        // All messages for this folder should be gone
        assert!(
            db.get_thread_messages(&thread.thread_id)
                .unwrap()
                .is_empty()
        );

        // last_uid should be reset to 0
        assert_eq!(db.get_last_synced_uid("acct1", "INBOX").unwrap(), Some(0));

        // New UIDVALIDITY should be stored
        assert_eq!(db.get_uidvalidity("acct1", "INBOX").unwrap(), Some(2000));

        // Thread with zero messages should have been cleaned up
        assert!(db.get_thread(&thread.thread_id).unwrap().is_none());
    }

    #[test]
    fn uidvalidity_reset_preserves_other_folders() {
        let db = Database::open_memory().unwrap();

        let thread = db
            .create_thread(
                "multi-folder",
                "2026-03-28T10:00:00",
                "2026-03-28T12:00:00",
                "acct1",
            )
            .unwrap();

        // Message in INBOX
        db.upsert_thread_message(
            &thread.thread_id,
            100,
            Some("<inbox-msg@x.com>"),
            None,
            None,
            "INBOX",
            "a@b.com",
            "c@d.com",
            "2026-03-28T10:00:00",
            "Test",
            false,
            None,
        )
        .unwrap();

        // Message in Sent
        db.upsert_thread_message(
            &thread.thread_id,
            50,
            Some("<sent-msg@x.com>"),
            None,
            None,
            "Sent",
            "c@d.com",
            "a@b.com",
            "2026-03-28T12:00:00",
            "Re: Test",
            true,
            None,
        )
        .unwrap();
        db.refresh_thread_stats(&thread.thread_id).unwrap();

        // Reset only INBOX
        let deleted = db.reset_folder_sync("acct1", "INBOX", 5000).unwrap();
        assert_eq!(deleted, 1);

        // Sent message should still exist
        let msgs = db.get_thread_messages(&thread.thread_id).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].folder, "Sent");

        // Thread should still exist (has messages from Sent)
        assert!(db.get_thread(&thread.thread_id).unwrap().is_some());
    }
}
