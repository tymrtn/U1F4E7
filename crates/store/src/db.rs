// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::errors::{Result, StoreError};
use crate::paths;
use rusqlite::{Connection, OpenFlags};

pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open or create the database at the default location.
    /// Default: ~/.config/envelope-email/envelope.db
    pub fn open_default() -> Result<Self> {
        let path = paths::database_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::Config(format!("cannot create config dir: {e}")))?;
        }
        let db = Self::open(&path)?;

        // Restrict database file to owner-only access
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&path, perms)
                .map_err(|e| StoreError::Config(format!("cannot set database permissions: {e}")))?;
        }

        Ok(db)
    }

    /// Open or create the database at a specific path.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        crate::migrations::run(&mut conn)?;
        Ok(Self { conn })
    }

    /// Open an existing database read-only without creating directories, creating
    /// a database file, switching journal mode, changing permissions, or running
    /// migrations. Returns `Ok(None)` when the default database path is absent.
    pub fn open_default_readonly_existing() -> Result<Option<Self>> {
        let path = paths::database_path();
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Some(Self { conn }))
    }

    /// Open an in-memory database (for testing).
    pub fn open_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        crate::migrations::run(&mut conn)?;
        Ok(Self { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    #[cfg(test)]
    pub(crate) fn test_insert_account_row(&self, id: &str, username: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
             imap_host, imap_port, encrypted_password)
             VALUES (?1, ?2, ?3, 'example.test', 'smtp.example.test', 587,
                     'imap.example.test', 993, 'encrypted')",
            rusqlite::params![id, username, username],
        )?;
        Ok(())
    }

    // ── Detected folder cache ────────────────────────────────────────

    /// Get the cached drafts folder name for an account.
    pub fn get_drafts_folder(&self, account_id: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let folder: Option<String> = self.conn.query_row(
            "SELECT folder_name FROM detected_folders WHERE account_id = ?1 AND folder_type = 'drafts'",
            rusqlite::params![account_id],
            |row| row.get(0),
        ).optional()?;
        Ok(folder)
    }

    /// Get the cached sent folder name for an account.
    pub fn get_sent_folder(&self, account_id: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let folder: Option<String> = self.conn.query_row(
            "SELECT folder_name FROM detected_folders WHERE account_id = ?1 AND folder_type = 'sent'",
            rusqlite::params![account_id],
            |row| row.get(0),
        ).optional()?;
        Ok(folder)
    }

    /// Cache a detected folder for an account.
    pub fn set_detected_folder(
        &self,
        account_id: &str,
        folder_type: &str,
        folder_name: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO detected_folders (account_id, folder_type, folder_name, detected_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(account_id, folder_type) DO UPDATE SET
                folder_name = excluded.folder_name,
                detected_at = excluded.detected_at",
            rusqlite::params![account_id, folder_type, folder_name],
        )?;
        Ok(())
    }

    /// Get all detected folders for an account.
    pub fn get_detected_folders(&self, account_id: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT folder_type, folder_name FROM detected_folders WHERE account_id = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![account_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // ── Provider type ────────────────────────────────────────────────

    /// Get the stored provider type for an account.
    /// Returns None if not yet detected (NULL in DB) or if account not found.
    pub fn get_provider_type(&self, account_id: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let row: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT provider_type FROM accounts WHERE id = ?1",
                rusqlite::params![account_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        // Flatten: None (no row) or Some(None) (NULL column) → None
        Ok(row.flatten())
    }

    /// Store the detected provider type for an account.
    pub fn set_provider_type(&self, account_id: &str, provider_type: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE accounts SET provider_type = ?2 WHERE id = ?1",
            rusqlite::params![account_id, provider_type],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::IndexedMessageInput;

    #[test]
    fn indexed_message_summaries_round_trip_and_sort_from_local_db() {
        let db = Database::open_memory().unwrap();
        db.test_insert_account_row("acct-a", "a@example.test")
            .unwrap();

        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            99,
            &[
                IndexedMessageInput {
                    uid: 10,
                    message_id: Some("<old@example.test>".to_string()),
                    from_addr: "old@example.test".to_string(),
                    to_addr: "me@example.test".to_string(),
                    subject: "old".to_string(),
                    date: Some("Tue, 12 May 2026 10:00:00 +0000".to_string()),
                    flags: vec!["\\Seen".to_string()],
                    size: 100,
                    snippet: Some("old preview".to_string()),
                    thread_id: Some("thread-old".to_string()),
                },
                IndexedMessageInput {
                    uid: 11,
                    message_id: Some("<new@example.test>".to_string()),
                    from_addr: "new@example.test".to_string(),
                    to_addr: "me@example.test".to_string(),
                    subject: "new".to_string(),
                    date: Some("Tue, 12 May 2026 12:00:00 +0000".to_string()),
                    flags: Vec::new(),
                    size: 200,
                    snippet: Some("new preview".to_string()),
                    thread_id: Some("thread-new".to_string()),
                },
            ],
        )
        .unwrap();

        let rows = db.list_indexed_message_summaries("INBOX", 10).unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].account_id, "acct-a");
        assert_eq!(rows[0].uidvalidity, 99);
        assert_eq!(rows[0].summary.uid, 11);
        assert_eq!(rows[0].snippet.as_deref(), Some("new preview"));
        assert_eq!(rows[0].thread_id.as_deref(), Some("thread-new"));
        assert_eq!(rows[0].freshness, "fresh");
        assert!(rows[0].indexed_at.is_some());

        db.upsert_indexed_message_summaries(
            "acct-a",
            "INBOX",
            99,
            &[IndexedMessageInput {
                uid: 12,
                message_id: Some("<replacement@example.test>".to_string()),
                from_addr: "replacement@example.test".to_string(),
                to_addr: "me@example.test".to_string(),
                subject: "replacement".to_string(),
                date: Some("Tue, 12 May 2026 13:00:00 +0000".to_string()),
                flags: Vec::new(),
                size: 300,
                snippet: None,
                thread_id: None,
            }],
        )
        .unwrap();

        let rows = db.list_indexed_message_summaries("INBOX", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary.uid, 12);
    }

    /// The shared on-disk database may have been advanced by the isolated V2
    /// runtime to its known additive schema level. V1 must open it and keep
    /// working against its own tables, without moving the schema version.
    #[test]
    fn open_works_on_known_forward_v2_schema_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forward.db");
        {
            let db = Database::open(&path).unwrap();
            crate::migrations::v2_fixture::apply(db.conn());
        }

        let db = Database::open(&path).unwrap();

        db.test_insert_account_row("acct-fwd", "fwd@example.test")
            .unwrap();
        db.set_detected_folder("acct-fwd", "drafts", "Drafts")
            .unwrap();
        assert_eq!(
            db.get_drafts_folder("acct-fwd").unwrap().as_deref(),
            Some("Drafts")
        );

        let version: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, crate::migrations::v2_fixture::V2_SCHEMA_VERSION);
    }
}
