// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Schema migrations for the Envelope SQLite database.
//!
//! Uses `rusqlite_migration` which tracks state via `PRAGMA user_version`.
//! Migration 0 is the baseline (v0.4.1 schema). All statements use
//! `IF NOT EXISTS` so they are safe for both fresh and existing databases.

use rusqlite::Connection;
use rusqlite::Transaction;
use rusqlite_migration::{M, Migrations};

/// Run all pending migrations on the given connection.
pub fn run(conn: &mut Connection) -> Result<(), rusqlite_migration::Error> {
    migrations().to_latest(conn)
}

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        // ── Migration 0: baseline (v0.4.1 schema) ──────────────────
        // All IF NOT EXISTS — safe for existing databases.
        M::up(
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
                provider_type TEXT,
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
                imap_uid INTEGER,
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

            CREATE TABLE IF NOT EXISTS detected_folders (
                account_id TEXT NOT NULL,
                folder_type TEXT NOT NULL,
                folder_name TEXT NOT NULL,
                detected_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (account_id, folder_type)
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
            CREATE INDEX IF NOT EXISTS idx_tags_tag
                ON message_tags(tag);
            CREATE INDEX IF NOT EXISTS idx_scores_dimension
                ON message_scores(dimension, value);
            CREATE INDEX IF NOT EXISTS idx_rules_account
                ON rules(account_id, enabled, priority);
            ",
        ),
        // ── Migration 1: idempotent column additions ────────────────
        // For databases created before these columns existed, the baseline
        // CREATE TABLE IF NOT EXISTS won't add them. This hook checks
        // pragma_table_info and adds missing columns.
        M::up_with_hook("", |tx: &Transaction| {
            let has_col = |table: &str, col: &str| -> bool {
                tx.prepare(&format!(
                    "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{col}'"
                ))
                .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
                .unwrap_or(0)
                    > 0
            };
            if !has_col("drafts", "imap_uid") {
                tx.execute_batch("ALTER TABLE drafts ADD COLUMN imap_uid INTEGER;")?;
            }
            if !has_col("accounts", "provider_type") {
                tx.execute_batch("ALTER TABLE accounts ADD COLUMN provider_type TEXT;")?;
            }
            Ok(())
        }),
        // ── Migration 2: events table (v0.5.0) ─────────────────────
        M::up(
            "
            CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                folder TEXT NOT NULL,
                uid INTEGER,
                message_id TEXT,
                from_addr TEXT,
                subject TEXT,
                snippet TEXT,
                payload TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_events_account_time
                ON events(account_id, created_at);
            CREATE INDEX IF NOT EXISTS idx_events_type
                ON events(event_type, created_at);
            ",
        ),
        // ── Migration 3: contacts table (v0.5.0) ───────────────────
        M::up(
            "
            CREATE TABLE IF NOT EXISTS contacts (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                email TEXT NOT NULL,
                name TEXT,
                tags TEXT NOT NULL DEFAULT '[]',
                notes TEXT,
                message_count INTEGER NOT NULL DEFAULT 0,
                first_seen TEXT,
                last_seen TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(account_id, email)
            );
            CREATE INDEX IF NOT EXISTS idx_contacts_account
                ON contacts(account_id);
            CREATE INDEX IF NOT EXISTS idx_contacts_email
                ON contacts(email);
            ",
        ),
        // ── Migration 4: event runtime primitives (v0.6.0) ────────
        M::up_with_hook("", |tx: &Transaction| {
            let has_col = |table: &str, col: &str| -> bool {
                tx.prepare(&format!(
                    "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{col}'"
                ))
                .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
                .unwrap_or(0)
                    > 0
            };

            if !has_col("events", "idempotency_key") {
                tx.execute_batch("ALTER TABLE events ADD COLUMN idempotency_key TEXT;")?;
            }
            if !has_col("events", "secure_pending") {
                tx.execute_batch(
                    "ALTER TABLE events ADD COLUMN secure_pending INTEGER NOT NULL DEFAULT 0;",
                )?;
            }
            if !has_col("events", "acked_at") {
                tx.execute_batch("ALTER TABLE events ADD COLUMN acked_at TEXT;")?;
            }
            if !has_col("action_log", "event_id") {
                tx.execute_batch("ALTER TABLE action_log ADD COLUMN event_id TEXT;")?;
            }
            if !has_col("action_log", "action_status") {
                tx.execute_batch(
                    "ALTER TABLE action_log ADD COLUMN action_status TEXT NOT NULL DEFAULT 'completed';",
                )?;
            }

            tx.execute_batch(
                "
                CREATE UNIQUE INDEX IF NOT EXISTS idx_events_idem
                    ON events(account_id, idempotency_key)
                    WHERE idempotency_key IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_events_unacked
                    ON events(account_id, acked_at, created_at)
                    WHERE acked_at IS NULL;
                CREATE UNIQUE INDEX IF NOT EXISTS idx_action_log_event_unique
                    ON action_log(event_id, action_type)
                    WHERE event_id IS NOT NULL;

                CREATE TABLE IF NOT EXISTS event_routes (
                    id TEXT PRIMARY KEY,
                    account_id TEXT NOT NULL,
                    match_expr TEXT NOT NULL,
                    delivery TEXT NOT NULL,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    priority INTEGER NOT NULL DEFAULT 100,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_event_routes_account
                    ON event_routes(account_id, enabled, priority);

                CREATE TABLE IF NOT EXISTS event_deliveries (
                    id TEXT PRIMARY KEY,
                    event_id TEXT NOT NULL,
                    route_id TEXT NOT NULL,
                    delivery_id TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending',
                    attempt_count INTEGER NOT NULL DEFAULT 0,
                    last_attempt_at TEXT,
                    error_summary TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    UNIQUE(event_id, route_id, delivery_id)
                );
                CREATE INDEX IF NOT EXISTS idx_event_deliveries_event
                    ON event_deliveries(event_id, status);
                ",
            )?;
            Ok(())
        }),
        // ── Migration 5: migration_uid_map (v0.6.0 mailbox migration) ──
        // Tracks copy-only IMAP-to-IMAP migrations so reruns are idempotent.
        // No deletes are ever recorded here — source mailboxes are never
        // mutated by the migrate command.
        M::up(
            "
            CREATE TABLE IF NOT EXISTS migration_uid_map (
                src_account_id TEXT NOT NULL,
                dst_account_id TEXT NOT NULL,
                src_folder TEXT NOT NULL,
                src_uid INTEGER NOT NULL,
                dst_folder TEXT NOT NULL,
                dst_uid INTEGER,
                message_id TEXT,
                size INTEGER,
                copied_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (src_account_id, dst_account_id, src_folder, src_uid)
            );
            CREATE INDEX IF NOT EXISTS idx_migration_uid_map_dst
                ON migration_uid_map(dst_account_id, dst_folder);
            CREATE INDEX IF NOT EXISTS idx_migration_uid_map_pair
                ON migration_uid_map(src_account_id, dst_account_id);
            ",
        ),
        // ── Migration 6: add source UIDVALIDITY to migration identity ──
        //
        // UID values are only stable within a folder's UIDVALIDITY epoch. Rebuild
        // the table so future migrations can safely copy the same numeric UID
        // after a source folder is recreated. Existing rows are assigned epoch 0
        // because older migration records did not know their source UIDVALIDITY.
        M::up_with_hook("", |tx: &Transaction| {
            let has_src_uidvalidity = tx
                .prepare(
                    "SELECT COUNT(*) FROM pragma_table_info('migration_uid_map')
                     WHERE name = 'src_uidvalidity'",
                )
                .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
                .unwrap_or(0)
                > 0;
            if has_src_uidvalidity {
                return Ok(());
            }

            tx.execute_batch(
                "
                ALTER TABLE migration_uid_map RENAME TO migration_uid_map_old;

                CREATE TABLE migration_uid_map (
                    src_account_id TEXT NOT NULL,
                    dst_account_id TEXT NOT NULL,
                    src_folder TEXT NOT NULL,
                    src_uidvalidity INTEGER NOT NULL DEFAULT 0,
                    src_uid INTEGER NOT NULL,
                    dst_folder TEXT NOT NULL,
                    dst_uidvalidity INTEGER,
                    dst_uid INTEGER,
                    message_id TEXT,
                    size INTEGER,
                    copied_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (
                        src_account_id,
                        dst_account_id,
                        src_folder,
                        src_uidvalidity,
                        src_uid
                    )
                );

                INSERT INTO migration_uid_map (
                    src_account_id,
                    dst_account_id,
                    src_folder,
                    src_uidvalidity,
                    src_uid,
                    dst_folder,
                    dst_uidvalidity,
                    dst_uid,
                    message_id,
                    size,
                    copied_at
                )
                SELECT
                    src_account_id,
                    dst_account_id,
                    src_folder,
                    0,
                    src_uid,
                    dst_folder,
                    NULL,
                    dst_uid,
                    message_id,
                    size,
                    copied_at
                FROM migration_uid_map_old;

                DROP TABLE migration_uid_map_old;

                CREATE INDEX IF NOT EXISTS idx_migration_uid_map_dst
                    ON migration_uid_map(dst_account_id, dst_folder);
                CREATE INDEX IF NOT EXISTS idx_migration_uid_map_pair
                    ON migration_uid_map(src_account_id, dst_account_id);
                ",
            )?;
            Ok(())
        }),
        // ── Migration 7: Agent Cockpit operational primitives ─────────
        M::up(
            "
            CREATE TABLE IF NOT EXISTS watch_registry (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                folder TEXT NOT NULL,
                status TEXT NOT NULL,
                process_id INTEGER,
                schedule TEXT,
                last_heartbeat_at TEXT,
                last_event_at TEXT,
                failure_reason TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(account_id, folder)
            );
            CREATE INDEX IF NOT EXISTS idx_watch_registry_account
                ON watch_registry(account_id, status, updated_at);

            CREATE TABLE IF NOT EXISTS failed_auth_history (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                backend TEXT NOT NULL,
                reason TEXT NOT NULL,
                retry_guidance TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_failed_auth_account
                ON failed_auth_history(account_id, created_at);

            CREATE TABLE IF NOT EXISTS rule_run_audit (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                rule_id TEXT,
                rule_name TEXT,
                uid INTEGER,
                folder TEXT,
                action TEXT,
                status TEXT NOT NULL,
                error TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_rule_run_audit_account
                ON rule_run_audit(account_id, created_at);
            CREATE INDEX IF NOT EXISTS idx_rule_run_audit_rule
                ON rule_run_audit(rule_id, created_at)
                WHERE rule_id IS NOT NULL;
            ",
        ),
        // ── Migration 8: indexed dashboard message-summary read model ──
        M::up(
            "
            CREATE TABLE IF NOT EXISTS indexed_message_summaries (
                account_id TEXT NOT NULL,
                folder TEXT NOT NULL,
                uidvalidity INTEGER NOT NULL,
                uid INTEGER NOT NULL,
                message_id TEXT,
                from_addr TEXT NOT NULL DEFAULT '',
                to_addr TEXT NOT NULL DEFAULT '',
                subject TEXT NOT NULL DEFAULT '',
                date TEXT,
                flags_json TEXT NOT NULL DEFAULT '[]',
                size INTEGER NOT NULL DEFAULT 0,
                snippet TEXT,
                thread_id TEXT,
                indexed_at TEXT NOT NULL,
                PRIMARY KEY (account_id, folder, uidvalidity, uid)
            );
            CREATE INDEX IF NOT EXISTS idx_indexed_message_summaries_folder_date
                ON indexed_message_summaries(folder, date DESC, uid DESC);
            CREATE INDEX IF NOT EXISTS idx_indexed_message_summaries_account_folder
                ON indexed_message_summaries(account_id, folder, indexed_at);
            CREATE INDEX IF NOT EXISTS idx_indexed_message_summaries_thread
                ON indexed_message_summaries(thread_id)
                WHERE thread_id IS NOT NULL;

            CREATE TABLE IF NOT EXISTS message_index_state (
                account_id TEXT NOT NULL,
                folder TEXT NOT NULL,
                uidvalidity INTEGER,
                indexed_at TEXT,
                last_error TEXT,
                PRIMARY KEY (account_id, folder)
            );
            ",
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_valid() {
        // rusqlite_migration validates that migrations are well-formed
        migrations().validate().unwrap();
    }

    #[test]
    fn fresh_database_migrates_cleanly() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();

        // Verify key tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"accounts".to_string()));
        assert!(tables.contains(&"events".to_string()));
        assert!(tables.contains(&"contacts".to_string()));
        assert!(tables.contains(&"rules".to_string()));
        assert!(tables.contains(&"event_routes".to_string()));
        assert!(tables.contains(&"event_deliveries".to_string()));
        assert!(tables.contains(&"migration_uid_map".to_string()));
    }

    #[test]
    fn migration_uid_map_columns_exist() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();

        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('migration_uid_map') ORDER BY cid")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        for required in [
            "src_account_id",
            "dst_account_id",
            "src_folder",
            "src_uidvalidity",
            "src_uid",
            "dst_folder",
            "dst_uidvalidity",
            "dst_uid",
            "message_id",
            "size",
            "copied_at",
        ] {
            assert!(
                columns.contains(&required.to_string()),
                "missing column: {required}"
            );
        }
    }

    #[test]
    fn migration_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        // Running again should be a no-op
        run(&mut conn).unwrap();
    }

    #[test]
    fn migration_uid_map_v5_rows_upgrade_with_epoch_zero() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE migration_uid_map (
                src_account_id TEXT NOT NULL,
                dst_account_id TEXT NOT NULL,
                src_folder TEXT NOT NULL,
                src_uid INTEGER NOT NULL,
                dst_folder TEXT NOT NULL,
                dst_uid INTEGER,
                message_id TEXT,
                size INTEGER,
                copied_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (src_account_id, dst_account_id, src_folder, src_uid)
            );
            INSERT INTO migration_uid_map (
                src_account_id, dst_account_id, src_folder, src_uid,
                dst_folder, dst_uid, message_id, size, copied_at
            ) VALUES ('a', 'b', 'INBOX', 42, 'INBOX', NULL, NULL, 10, '2026-01-01 00:00:00');
            PRAGMA user_version = 6;
            ",
        )
        .unwrap();

        run(&mut conn).unwrap();

        let row: (i64, Option<i64>) = conn
            .query_row(
                "SELECT src_uidvalidity, dst_uidvalidity FROM migration_uid_map
                 WHERE src_account_id = 'a' AND dst_account_id = 'b'
                   AND src_folder = 'INBOX' AND src_uid = 42",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (0, None));

        conn.execute(
            "INSERT INTO migration_uid_map (
                src_account_id, dst_account_id, src_folder, src_uidvalidity, src_uid,
                dst_folder, dst_uidvalidity, dst_uid, message_id, size
             ) VALUES ('a', 'b', 'INBOX', 999, 42, 'INBOX', NULL, NULL, NULL, 10)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn event_runtime_columns_exist() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();

        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('events') ORDER BY cid")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|row| row.ok())
            .collect();

        assert!(columns.contains(&"idempotency_key".to_string()));
        assert!(columns.contains(&"secure_pending".to_string()));
        assert!(columns.contains(&"acked_at".to_string()));
    }
}
