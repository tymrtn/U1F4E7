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
        // ── Migration 9: agent identities (v2 multi-agent attribution) ──
        // Per-agent identity + policy for a shared inbox. Every action is
        // attributed via the additive agent_id columns on action_log/events
        // (NULL = human/legacy; existing rows are never rewritten). Only a
        // SHA-256 hash of each bearer token is stored — never the raw token.
        M::up_with_hook(
            "
            CREATE TABLE IF NOT EXISTS agent_identities (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                token_hash TEXT NOT NULL UNIQUE,
                token_prefix TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                revoked_at TEXT,
                last_used_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_agent_identities_active
                ON agent_identities(revoked_at);

            CREATE TABLE IF NOT EXISTS agent_policies (
                agent_id TEXT PRIMARY KEY REFERENCES agent_identities(id) ON DELETE CASCADE,
                allowed_accounts TEXT NOT NULL DEFAULT '*',
                allowed_folders TEXT NOT NULL DEFAULT '*',
                allowed_actions TEXT NOT NULL DEFAULT '*',
                send_mode_ceiling TEXT NOT NULL DEFAULT 'draft-only',
                allow_recipients TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            ",
            // Additive agent_id columns on the audit tables. NULL means the
            // action was taken by a human or predates multi-agent attribution;
            // existing rows are left untouched. Guarded on table existence so a
            // database that skipped the baseline (e.g. a partial fixture) still
            // migrates cleanly.
            |tx: &Transaction| {
                let table_exists = |table: &str| -> bool {
                    tx.prepare(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    )
                    .and_then(|mut s| s.query_row([table], |row| row.get::<_, i64>(0)))
                    .unwrap_or(0)
                        > 0
                };
                let has_col = |table: &str, col: &str| -> bool {
                    tx.prepare(&format!(
                        "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{col}'"
                    ))
                    .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
                    .unwrap_or(0)
                        > 0
                };
                if table_exists("action_log") && !has_col("action_log", "agent_id") {
                    tx.execute_batch("ALTER TABLE action_log ADD COLUMN agent_id TEXT;")?;
                }
                if table_exists("events") && !has_col("events", "agent_id") {
                    tx.execute_batch("ALTER TABLE events ADD COLUMN agent_id TEXT;")?;
                }
                Ok(())
            },
        ),
        // ── Migration 10: durable event push (v2 webhook delivery) ──────
        // Turns the fire-and-forget webhook path into an at-least-once
        // delivery pipeline. Additive only — new columns default to values
        // that preserve the meaning of pre-existing rows:
        //   * event_routes.secret        HMAC-SHA256 signing key per route,
        //                                generated once at creation, shown once.
        //   * event_deliveries.next_attempt_at  when the delivery is next due.
        //   * event_deliveries.last_status_code HTTP status of last attempt.
        //   * event_deliveries.last_response_snippet  response body, capped at
        //                                RESPONSE_SNIPPET_CAP_BYTES (1 KiB) to
        //                                bound storage and avoid logging secrets.
        //   * event_deliveries.last_error        transport-level error string.
        //   * event_deliveries.dead_lettered_at  set once retries are exhausted.
        //   * event_deliveries.delivered_at      set on the first 2xx response.
        // Every ALTER is guarded on column existence so a database that skipped
        // the baseline still migrates cleanly (same pattern as migration 9).
        M::up_with_hook("", |tx: &Transaction| {
            let table_exists = |table: &str| -> bool {
                tx.prepare("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
                    .and_then(|mut s| s.query_row([table], |row| row.get::<_, i64>(0)))
                    .unwrap_or(0)
                    > 0
            };
            let has_col = |table: &str, col: &str| -> bool {
                tx.prepare(&format!(
                    "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{col}'"
                ))
                .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
                .unwrap_or(0)
                    > 0
            };

            if table_exists("event_routes") && !has_col("event_routes", "secret") {
                tx.execute_batch("ALTER TABLE event_routes ADD COLUMN secret TEXT;")?;
            }

            if table_exists("event_deliveries") {
                for (col, ddl) in [
                    (
                        "next_attempt_at",
                        "ALTER TABLE event_deliveries ADD COLUMN next_attempt_at TEXT;",
                    ),
                    (
                        "last_status_code",
                        "ALTER TABLE event_deliveries ADD COLUMN last_status_code INTEGER;",
                    ),
                    (
                        "last_response_snippet",
                        "ALTER TABLE event_deliveries ADD COLUMN last_response_snippet TEXT;",
                    ),
                    (
                        "last_error",
                        "ALTER TABLE event_deliveries ADD COLUMN last_error TEXT;",
                    ),
                    (
                        "dead_lettered_at",
                        "ALTER TABLE event_deliveries ADD COLUMN dead_lettered_at TEXT;",
                    ),
                    (
                        "delivered_at",
                        "ALTER TABLE event_deliveries ADD COLUMN delivered_at TEXT;",
                    ),
                ] {
                    if !has_col("event_deliveries", col) {
                        tx.execute_batch(ddl)?;
                    }
                }
                // Due-work index: scan pending, not-yet-delivered, not
                // dead-lettered deliveries whose next attempt is due.
                tx.execute_batch(
                    "CREATE INDEX IF NOT EXISTS idx_event_deliveries_due
                        ON event_deliveries(next_attempt_at)
                        WHERE delivered_at IS NULL AND dead_lettered_at IS NULL;",
                )?;
            }
            Ok(())
        }),
        // ── Migration 11: draft revision counter (approval binding) ─────
        // Monotonic per-draft revision, bumped in the same UPDATE statement
        // as every content-relevant mutation (content/recipients,
        // attachments, metadata). The human-approval attestation records the
        // revision it approved; approval derives valid only while the
        // draft's revision still matches, and the approval write is
        // compare-and-set on this column so a concurrent edit can never
        // inherit an approval. Additive; existing rows start at revision 0.
        M::up_with_hook("", |tx: &Transaction| {
            let table_exists = |table: &str| -> bool {
                tx.prepare("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
                    .and_then(|mut s| s.query_row([table], |row| row.get::<_, i64>(0)))
                    .unwrap_or(0)
                    > 0
            };
            let has_col = |table: &str, col: &str| -> bool {
                tx.prepare(&format!(
                    "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{col}'"
                ))
                .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
                .unwrap_or(0)
                    > 0
            };
            if table_exists("drafts") && !has_col("drafts", "revision") {
                tx.execute_batch(
                    "ALTER TABLE drafts ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;",
                )?;
            }
            Ok(())
        }),
        // ── Migration 12: draft operation lease token ────────────────────
        // Opaque owner token for the durable `sending`/`syncing` claims.
        // Claim primitives generate it; mark-sent/release/finalize require
        // id + token, so a non-owner can neither finalize nor release another
        // actor's claim. NULL means no active lease (the token is cleared on
        // every terminal/released transition). Additive; existing rows —
        // including any legacy stranded `sending` rows — start with NULL and
        // stay inert until repaired.
        M::up_with_hook("", |tx: &Transaction| {
            let table_exists = |table: &str| -> bool {
                tx.prepare("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
                    .and_then(|mut s| s.query_row([table], |row| row.get::<_, i64>(0)))
                    .unwrap_or(0)
                    > 0
            };
            let has_col = |table: &str, col: &str| -> bool {
                tx.prepare(&format!(
                    "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{col}'"
                ))
                .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
                .unwrap_or(0)
                    > 0
            };
            if table_exists("drafts") && !has_col("drafts", "operation_token") {
                tx.execute_batch("ALTER TABLE drafts ADD COLUMN operation_token TEXT;")?;
            }
            Ok(())
        }),
        // ── Migration 13: address history cache (compose autocomplete) ──
        // `address_history_state` is the per-account reconciliation boundary:
        // `last_thread_message_id` is how far up the monotonic
        // `thread_messages.id` sequence the address history has been folded
        // in, so a refresh never rescans the deep thread cache;
        // `source_version` forces a rebuild when the derivation itself
        // changes; and `dirty` forces one when the source rows change under
        // the watermark (an in-place header rewrite, or the delete/reinsert a
        // UIDVALIDITY reset performs). `contacts.history_count` is the derived
        // half of the interaction signal, kept apart from the `message_count`
        // that `envelope contacts import` and manual edits own.
        //
        // `contacts.history_derived` is who owns the row. A row the derivation
        // invented is marked 1 and may be dropped again when its last source
        // disappears — a header a rescan corrected, say. Anything `envelope
        // contacts` created or edited is 0 and is never deleted by a rebuild,
        // however little signal it carries. The default is 0, so every contact
        // that predates this migration is treated as manually managed: an
        // upgrade must not delete a row it cannot prove it invented.
        //
        // `thread_messages` also gains `cc_addresses`/`bcc_addresses`: the
        // scan already parses those headers, and a Cc recipient is history
        // worth suggesting. They are NULL on every row written before this
        // migration — nothing backfills them, and they fill in as read-only
        // scans revisit each folder.
        //
        // All additive; existing rows start at zero/NULL and the first
        // reconcile folds their history in from the start.
        M::up_with_hook("", |tx: &Transaction| {
            let table_exists = |table: &str| -> bool {
                tx.prepare("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
                    .and_then(|mut s| s.query_row([table], |row| row.get::<_, i64>(0)))
                    .unwrap_or(0)
                    > 0
            };
            let has_col = |table: &str, col: &str| -> bool {
                tx.prepare(&format!(
                    "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{col}'"
                ))
                .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
                .unwrap_or(0)
                    > 0
            };
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS address_history_state (
                    account_id TEXT PRIMARY KEY,
                    source_version INTEGER NOT NULL DEFAULT 0,
                    last_thread_message_id INTEGER NOT NULL DEFAULT 0,
                    reconciled_at TEXT NOT NULL DEFAULT (datetime('now')),
                    dirty INTEGER NOT NULL DEFAULT 0
                );",
            )?;
            if table_exists("contacts") {
                if !has_col("contacts", "history_count") {
                    tx.execute_batch(
                        "ALTER TABLE contacts ADD COLUMN history_count INTEGER NOT NULL DEFAULT 0;",
                    )?;
                }
                if !has_col("contacts", "history_derived") {
                    tx.execute_batch(
                        "ALTER TABLE contacts ADD COLUMN history_derived INTEGER NOT NULL DEFAULT 0;",
                    )?;
                }
            }
            if table_exists("thread_messages") {
                if !has_col("thread_messages", "cc_addresses") {
                    tx.execute_batch("ALTER TABLE thread_messages ADD COLUMN cc_addresses TEXT;")?;
                }
                if !has_col("thread_messages", "bcc_addresses") {
                    tx.execute_batch("ALTER TABLE thread_messages ADD COLUMN bcc_addresses TEXT;")?;
                }
            }
            Ok(())
        }),
        // ── Migration 14: immediate post-send suggestibility ────────
        // `contacts.history_sent_count` is the locally recorded sent-draft
        // half of the derived signal, kept in its own column so the write
        // that runs the instant a send is durable cannot stack with the
        // reconcile's later accounting of the same message.
        //
        // A send transitions its draft to `sent` and folds that draft's
        // recipients in immediately, so they are suggestible without waiting
        // for a thread scan or a refresh. When the Sent-folder copy is later
        // cached and a reconcile folds it in, that message reaches
        // `history_count` through the ordinary thread/recount path — and
        // because the immediate write never touched `history_count`, the
        // count comes out exactly as it would have without it. The two
        // columns are separate sources, never one running total.
        //
        // Both writes are recomputed floors rather than increments: the
        // immediate edge recounts the same sent-draft window the reconcile
        // reads, so running it twice for one send changes nothing. A rebuild
        // resets this column alongside `history_count`, which is what lets a
        // derived row whose last source is gone still be swept.
        //
        // Additive; existing rows start at zero and the next completed
        // reconcile fills the column in. No source-version bump: the value is
        // subsumed by `history_count` once a reconcile has run, so it changes
        // no ranking and is not worth re-folding six figures of thread rows
        // for.
        M::up_with_hook("", |tx: &Transaction| {
            let has_col = |table: &str, col: &str| -> bool {
                tx.prepare(&format!(
                    "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{col}'"
                ))
                .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
                .unwrap_or(0)
                    > 0
            };
            let table_exists = tx
                .prepare("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
                .and_then(|mut s| s.query_row(["contacts"], |row| row.get::<_, i64>(0)))
                .unwrap_or(0)
                > 0;
            if table_exists && !has_col("contacts", "history_sent_count") {
                tx.execute_batch(
                    "ALTER TABLE contacts ADD COLUMN history_sent_count INTEGER NOT NULL DEFAULT 0;",
                )?;
            }
            Ok(())
        }),
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

    /// Regression for migration 11: a pre-revision draft row (database created
    /// before the revision column existed) upgrades in place and reads back as
    /// revision 0 — the value the approval CAS and claim primitives treat as
    /// the row's first revision.
    #[test]
    fn pre_revision_draft_rows_upgrade_to_revision_zero() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE drafts (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
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
                created_by TEXT,
                imap_uid INTEGER
            );
            INSERT INTO drafts (id, account_id, to_addr, subject)
                VALUES ('d-legacy', 'acc', 'to@example.net', 'Pre-revision draft');
            PRAGMA user_version = 11;
            ",
        )
        .unwrap();

        run(&mut conn).unwrap();

        let revision: i64 = conn
            .query_row(
                "SELECT revision FROM drafts WHERE id = 'd-legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(revision, 0, "legacy rows start at revision 0");
    }

    /// Regression for migration 12: pre-lease rows upgrade in place with a
    /// NULL operation token — no active lease, inert until claimed anew.
    #[test]
    fn pre_lease_draft_rows_upgrade_with_null_operation_token() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE drafts (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'draft',
                to_addr TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO drafts (id, account_id, to_addr, status)
                VALUES ('d-legacy', 'acc', 'to@example.net', 'sending');
            PRAGMA user_version = 12;
            ",
        )
        .unwrap();

        run(&mut conn).unwrap();

        let token: Option<String> = conn
            .query_row(
                "SELECT operation_token FROM drafts WHERE id = 'd-legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            token.is_none(),
            "legacy rows (even stranded `sending` ones) carry no lease"
        );
    }

    /// Regression for migrations 13 and 14: an install that already has
    /// contacts — counts and curation included — gains both derived columns at
    /// zero and an empty reconciliation boundary, so the first reconcile folds
    /// its history in from the start without disturbing what is already there.
    /// Its cached thread rows gain empty Cc/Bcc columns: nothing backfills
    /// those, they fill in as read-only scans revisit each folder.
    #[test]
    fn pre_address_history_contacts_upgrade_with_a_zero_derived_count() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE contacts (
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
            INSERT INTO contacts (id, account_id, email, name, tags, notes, message_count)
                VALUES ('c-legacy', 'acc', 'imported@example.net', 'Imported',
                        '[\"vendor\"]', 'Net-30', 87);
            CREATE TABLE thread_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                thread_id TEXT NOT NULL,
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
            INSERT INTO thread_messages (thread_id, uid, folder, from_address, to_addresses)
                VALUES ('t-legacy', 1, 'INBOX', 'ada@example.net', 'me@example.net');
            PRAGMA user_version = 12;
            ",
        )
        .unwrap();

        run(&mut conn).unwrap();

        let (message_count, history_count, history_sent_count, history_derived, tags, notes): (
            i64,
            i64,
            i64,
            i64,
            String,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT message_count, history_count, history_sent_count, history_derived,
                        tags, notes
                 FROM contacts WHERE id = 'c-legacy'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(message_count, 87, "an imported count survives the upgrade");
        assert_eq!(history_count, 0, "nothing has been derived locally yet");
        assert_eq!(
            history_sent_count, 0,
            "and nothing has been sent through this install yet either"
        );
        assert_eq!(
            history_derived, 0,
            "a contact that predates the derivation is manually managed, and a \
             rebuild may never delete it"
        );
        assert_eq!(tags, r#"["vendor"]"#);
        assert_eq!(notes.as_deref(), Some("Net-30"));

        let boundaries: i64 = conn
            .query_row("SELECT COUNT(*) FROM address_history_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(boundaries, 0, "no account has been reconciled yet");

        // The reconciliation boundary carries the invalidation flag the
        // mutation paths set, defaulted off.
        conn.execute(
            "INSERT INTO address_history_state (account_id, source_version, last_thread_message_id)
             VALUES ('acc', 1, 5)",
            [],
        )
        .unwrap();
        let dirty: i64 = conn
            .query_row(
                "SELECT dirty FROM address_history_state WHERE account_id = 'acc'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dirty, 0);

        let (cc, bcc): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT cc_addresses, bcc_addresses FROM thread_messages WHERE uid = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (cc, bcc),
            (None, None),
            "an already-cached thread row has no Cc/Bcc to recover; a later \
             read-only scan is what fills these in"
        );
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
    fn migration_10_adds_delivery_result_and_retry_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();

        let cols = |conn: &Connection, table: &str| -> Vec<String> {
            conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };

        let route_cols = cols(&conn, "event_routes");
        assert!(
            route_cols.contains(&"secret".to_string()),
            "event_routes must carry a per-route HMAC secret column"
        );

        let delivery_cols = cols(&conn, "event_deliveries");
        for required in [
            "attempt_count",
            "next_attempt_at",
            "last_status_code",
            "last_response_snippet",
            "last_error",
            "dead_lettered_at",
            "delivered_at",
        ] {
            assert!(
                delivery_cols.contains(&required.to_string()),
                "event_deliveries missing column: {required}"
            );
        }
    }

    #[test]
    fn migration_10_guard_survives_rerun() {
        // Re-running after every column already exists must be a no-op; the
        // guard must not assume a fresh schema.
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
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
    fn agent_identity_tables_and_columns_exist() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.contains(&"agent_identities".to_string()));
        assert!(tables.contains(&"agent_policies".to_string()));

        let has_col = |table: &str, col: &str| -> bool {
            let n: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{col}'"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            n > 0
        };
        assert!(has_col("action_log", "agent_id"));
        assert!(has_col("events", "agent_id"));
    }

    #[test]
    fn agent_id_columns_apply_to_pre_migration_database() {
        // Simulate a database created before migration 9: build the v0.6 audit
        // tables by hand, seed a legacy row, then run migrations to latest.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE action_log (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                action_type TEXT NOT NULL,
                confidence REAL NOT NULL DEFAULT 0.0,
                justification TEXT NOT NULL DEFAULT '',
                action_taken TEXT NOT NULL DEFAULT '',
                message_id TEXT,
                draft_id TEXT,
                event_id TEXT,
                action_status TEXT NOT NULL DEFAULT 'completed',
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE events (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                folder TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            INSERT INTO action_log (id, account_id, action_type) VALUES ('a-legacy', 'acc', 'x');
            INSERT INTO events (id, account_id, event_type, folder)
                VALUES ('e-legacy', 'acc', 'new_message', 'INBOX');
            PRAGMA user_version = 8;
            ",
        )
        .unwrap();

        run(&mut conn).unwrap();

        // Legacy rows survive and their new agent_id column is NULL.
        let action_agent: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM action_log WHERE id = 'a-legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(action_agent, None);
        let event_agent: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM events WHERE id = 'e-legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_agent, None);
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
