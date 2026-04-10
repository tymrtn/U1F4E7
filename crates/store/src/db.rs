// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::errors::{Result, StoreError};
use rusqlite::Connection;
use std::path::PathBuf;
use tracing::info;

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
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Open an in-memory database (for testing).
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    fn default_path() -> PathBuf {
        let config_dir = dirs_next::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
        config_dir.join("envelope-email").join("envelope.db")
    }

    fn migrate(&self) -> Result<()> {
        info!("running database migrations");
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS accounts (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                username TEXT NOT NULL UNIQUE,
                domain TEXT NOT NULL,
                smtp_host TEXT NOT NULL,
                smtp_port INTEGER NOT NULL DEFAULT 587,
                imap_host TEXT NOT NULL,
                imap_port INTEGER NOT NULL DEFAULT 993,
                smtp_username TEXT,
                imap_username TEXT,
                display_name TEXT,
                encrypted_password TEXT NOT NULL,
                encrypted_smtp_password TEXT,
                encrypted_imap_password TEXT,
                signature_text TEXT,
                signature_html TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS drafts (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL REFERENCES accounts(id),
                status TEXT NOT NULL DEFAULT 'draft',
                to_addr TEXT NOT NULL,
                cc_addr TEXT,
                bcc_addr TEXT,
                reply_to TEXT,
                subject TEXT,
                text_content TEXT,
                html_content TEXT,
                in_reply_to TEXT,
                metadata TEXT,
                attachments TEXT NOT NULL DEFAULT '[]',
                message_id TEXT,
                send_after TEXT,
                snoozed_until TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                sent_at TEXT,
                created_by TEXT
            );

            CREATE TABLE IF NOT EXISTS action_log (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                action_type TEXT NOT NULL,
                confidence REAL NOT NULL DEFAULT 0.0,
                justification TEXT NOT NULL DEFAULT '',
                action_taken TEXT NOT NULL DEFAULT '',
                message_id TEXT,
                draft_id TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS license_keys (
                id TEXT PRIMARY KEY,
                token TEXT NOT NULL,
                licensee TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                features TEXT NOT NULL DEFAULT '[]',
                activated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS snoozed (
                id TEXT PRIMARY KEY,
                account TEXT NOT NULL,
                uid INTEGER NOT NULL,
                original_folder TEXT NOT NULL,
                snoozed_folder TEXT NOT NULL,
                return_at TEXT NOT NULL,
                message_id TEXT,
                subject TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                reason TEXT,
                note TEXT,
                recipient TEXT,
                escalation_tier INTEGER NOT NULL DEFAULT 0,
                reply_received INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS threads (
                thread_id TEXT PRIMARY KEY,
                subject_normalized TEXT NOT NULL,
                first_seen TEXT NOT NULL,
                last_activity TEXT NOT NULL,
                message_count INTEGER NOT NULL DEFAULT 0,
                account_id TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS thread_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                thread_id TEXT NOT NULL REFERENCES threads(thread_id),
                uid INTEGER NOT NULL,
                message_id TEXT,
                in_reply_to TEXT,
                reference_ids TEXT,
                folder TEXT NOT NULL,
                from_address TEXT,
                to_addresses TEXT,
                date TEXT,
                subject TEXT,
                is_outbound INTEGER NOT NULL DEFAULT 0,
                snippet TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_drafts_account_status
                ON drafts(account_id, status);
            CREATE INDEX IF NOT EXISTS idx_drafts_send_after
                ON drafts(send_after) WHERE status = 'draft';
            CREATE INDEX IF NOT EXISTS idx_action_log_account
                ON action_log(account_id, created_at);
            CREATE INDEX IF NOT EXISTS idx_snoozed_return_at
                ON snoozed(return_at);
            CREATE INDEX IF NOT EXISTS idx_snoozed_account_uid
                ON snoozed(account, uid);
            CREATE INDEX IF NOT EXISTS idx_threads_account
                ON threads(account_id, last_activity);
            CREATE INDEX IF NOT EXISTS idx_thread_messages_thread
                ON thread_messages(thread_id);
            CREATE INDEX IF NOT EXISTS idx_thread_messages_uid
                ON thread_messages(uid, folder);

            CREATE TABLE IF NOT EXISTS thread_sync_state (
                account_id TEXT NOT NULL,
                folder TEXT NOT NULL,
                last_uid INTEGER NOT NULL DEFAULT 0,
                uidvalidity INTEGER,
                synced_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (account_id, folder)
            );

            CREATE TABLE IF NOT EXISTS message_tags (
                account_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                tag TEXT NOT NULL,
                uid INTEGER,
                folder TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (account_id, message_id, tag)
            );
            CREATE INDEX IF NOT EXISTS idx_tags_tag ON message_tags(tag);

            CREATE TABLE IF NOT EXISTS message_scores (
                account_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                dimension TEXT NOT NULL,
                value REAL NOT NULL,
                uid INTEGER,
                folder TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (account_id, message_id, dimension)
            );
            CREATE INDEX IF NOT EXISTS idx_scores_dimension
                ON message_scores(dimension, value);

            CREATE TABLE IF NOT EXISTS rules (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                name TEXT NOT NULL,
                match_expr TEXT NOT NULL,
                action TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                priority INTEGER NOT NULL DEFAULT 100,
                stop INTEGER NOT NULL DEFAULT 0,
                sieve_exportable INTEGER NOT NULL DEFAULT 0,
                hit_count INTEGER NOT NULL DEFAULT 0,
                last_hit_at TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(account_id, name)
            );
            CREATE INDEX IF NOT EXISTS idx_rules_account
                ON rules(account_id, enabled, priority);
            ",
        )?;

        // Add imap_uid column for IMAP-first drafts (idempotent for existing DBs).
        let has_imap_uid: bool = self
            .conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('drafts') WHERE name = 'imap_uid'")?
            .query_row([], |row| row.get::<_, i64>(0))
            .unwrap_or(0)
            > 0;
        if !has_imap_uid {
            self.conn
                .execute_batch("ALTER TABLE drafts ADD COLUMN imap_uid INTEGER;")?;
        }

        // Create detected_folders table for caching auto-detected folder names
        // (drafts, sent, trash, etc.) per account.
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS detected_folders (
                account_id TEXT NOT NULL,
                folder_type TEXT NOT NULL,
                folder_name TEXT NOT NULL,
                detected_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (account_id, folder_type)
            );
            ",
        )?;

        // Add provider_type column to accounts table (idempotent for existing DBs).
        // Stores the detected email provider type (gmail, standard, dovecot, exchange, unknown).
        // NULL means not yet detected — triggers auto-detection on first IMAP connection.
        let has_provider_type: bool = self
            .conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('accounts') WHERE name = 'provider_type'",
            )?
            .query_row([], |row| row.get::<_, i64>(0))
            .unwrap_or(0)
            > 0;
        if !has_provider_type {
            self.conn
                .execute_batch("ALTER TABLE accounts ADD COLUMN provider_type TEXT;")?;
        }

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
