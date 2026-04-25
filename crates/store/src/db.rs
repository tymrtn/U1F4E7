// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::errors::{Result, StoreError};
use rusqlite::Connection;
use std::path::PathBuf;

pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open or create the database at the default location.
    /// Default: ~/.config/envelope-email/envelope.db
    pub fn open_default() -> Result<Self> {
        let path = Self::default_path();
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
        crate::migrations::run(&mut conn).map_err(|e| StoreError::Migration(format!("{e}")))?;
        Ok(Self { conn })
    }

    /// Open an in-memory database (for testing).
    pub fn open_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        crate::migrations::run(&mut conn).map_err(|e| StoreError::Migration(format!("{e}")))?;
        Ok(Self { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    fn default_path() -> PathBuf {
        let config_dir = dirs_next::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
        config_dir.join("envelope-email").join("envelope.db")
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
