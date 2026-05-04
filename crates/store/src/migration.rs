// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Persistence for IMAP-to-IMAP mailbox migrations.
//!
//! The `migration_uid_map` table records every successfully copied message so
//! reruns of `envelope migrate run` skip work already done. Migration is
//! copy-only — this table tracks copies, never deletions.

use crate::db::Database;
use crate::errors::Result;
use rusqlite::{OptionalExtension, params};

/// One row in the `migration_uid_map` table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MigrationUidEntry {
    pub src_account_id: String,
    pub dst_account_id: String,
    pub src_folder: String,
    pub src_uidvalidity: u32,
    pub src_uid: u32,
    pub dst_folder: String,
    pub dst_uidvalidity: Option<u32>,
    pub dst_uid: Option<u32>,
    pub message_id: Option<String>,
    pub size: Option<u64>,
    pub copied_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationKey<'a> {
    pub src_account_id: &'a str,
    pub dst_account_id: &'a str,
    pub src_folder: &'a str,
    pub src_uidvalidity: u32,
    pub src_uid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationScope<'a> {
    pub src_account_id: &'a str,
    pub dst_account_id: &'a str,
    pub src_folder: Option<&'a str>,
    pub src_uidvalidity: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationRecord<'a> {
    pub key: MigrationKey<'a>,
    pub dst_folder: &'a str,
    pub dst_uidvalidity: Option<u32>,
    pub dst_uid: Option<u32>,
    pub message_id: Option<&'a str>,
    pub size: Option<u64>,
}

impl Database {
    /// Insert (or replace) a migration record for a successfully copied message.
    pub fn record_migration(&self, record: MigrationRecord<'_>) -> Result<()> {
        self.conn().execute(
            "INSERT INTO migration_uid_map (
                src_account_id, dst_account_id, src_folder, src_uidvalidity, src_uid,
                dst_folder, dst_uidvalidity, dst_uid, message_id, size
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(src_account_id, dst_account_id, src_folder, src_uidvalidity, src_uid)
             DO UPDATE SET
                dst_folder = excluded.dst_folder,
                dst_uidvalidity = excluded.dst_uidvalidity,
                dst_uid = excluded.dst_uid,
                message_id = excluded.message_id,
                size = excluded.size,
                copied_at = datetime('now')",
            params![
                record.key.src_account_id,
                record.key.dst_account_id,
                record.key.src_folder,
                record.key.src_uidvalidity,
                record.key.src_uid,
                record.dst_folder,
                record.dst_uidvalidity,
                record.dst_uid,
                record.message_id,
                record.size.map(|s| s as i64),
            ],
        )?;
        Ok(())
    }

    /// Has this source UID, in this UIDVALIDITY epoch, already been migrated?
    pub fn is_migrated(&self, key: MigrationKey<'_>) -> Result<bool> {
        let exists: Option<i64> = self
            .conn()
            .query_row(
                "SELECT 1 FROM migration_uid_map
                 WHERE src_account_id = ?1 AND dst_account_id = ?2
                   AND src_folder = ?3 AND src_uidvalidity = ?4 AND src_uid = ?5",
                params![
                    key.src_account_id,
                    key.dst_account_id,
                    key.src_folder,
                    key.src_uidvalidity,
                    key.src_uid
                ],
                |row| row.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    /// Count migrated messages for a source/destination pair, optionally scoped
    /// to a folder and UIDVALIDITY epoch.
    pub fn count_migrated(&self, scope: MigrationScope<'_>) -> Result<u64> {
        let n: i64 = match (scope.src_folder, scope.src_uidvalidity) {
            (Some(folder), Some(uidvalidity)) => self.conn().query_row(
                "SELECT COUNT(*) FROM migration_uid_map
                 WHERE src_account_id = ?1 AND dst_account_id = ?2
                   AND src_folder = ?3 AND src_uidvalidity = ?4",
                params![
                    scope.src_account_id,
                    scope.dst_account_id,
                    folder,
                    uidvalidity
                ],
                |row| row.get(0),
            )?,
            (Some(folder), None) => self.conn().query_row(
                "SELECT COUNT(*) FROM migration_uid_map
                 WHERE src_account_id = ?1 AND dst_account_id = ?2 AND src_folder = ?3",
                params![scope.src_account_id, scope.dst_account_id, folder],
                |row| row.get(0),
            )?,
            (None, Some(uidvalidity)) => self.conn().query_row(
                "SELECT COUNT(*) FROM migration_uid_map
                 WHERE src_account_id = ?1 AND dst_account_id = ?2 AND src_uidvalidity = ?3",
                params![scope.src_account_id, scope.dst_account_id, uidvalidity],
                |row| row.get(0),
            )?,
            (None, None) => self.conn().query_row(
                "SELECT COUNT(*) FROM migration_uid_map
                 WHERE src_account_id = ?1 AND dst_account_id = ?2",
                params![scope.src_account_id, scope.dst_account_id],
                |row| row.get(0),
            )?,
        };
        Ok(n as u64)
    }

    /// List all migrated UIDs for a given source folder/pair.
    pub fn list_migrated_uids(&self, scope: MigrationScope<'_>) -> Result<Vec<u32>> {
        let Some(src_folder) = scope.src_folder else {
            return Ok(Vec::new());
        };

        let mut stmt = self.conn().prepare(
            "SELECT src_uid FROM migration_uid_map
             WHERE src_account_id = ?1 AND dst_account_id = ?2
               AND src_folder = ?3 AND (?4 IS NULL OR src_uidvalidity = ?4)
             ORDER BY src_uid",
        )?;
        let rows = stmt.query_map(
            params![
                scope.src_account_id,
                scope.dst_account_id,
                src_folder,
                scope.src_uidvalidity
            ],
            |row| row.get::<_, i64>(0).map(|v| v as u32),
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key<'a>(uidvalidity: u32, uid: u32) -> MigrationKey<'a> {
        MigrationKey {
            src_account_id: "a",
            dst_account_id: "b",
            src_folder: "INBOX",
            src_uidvalidity: uidvalidity,
            src_uid: uid,
        }
    }

    fn scope(uidvalidity: Option<u32>) -> MigrationScope<'static> {
        MigrationScope {
            src_account_id: "a",
            dst_account_id: "b",
            src_folder: Some("INBOX"),
            src_uidvalidity: uidvalidity,
        }
    }

    fn record(key: MigrationKey<'_>) -> MigrationRecord<'_> {
        MigrationRecord {
            key,
            dst_folder: "INBOX",
            dst_uidvalidity: Some(200),
            dst_uid: None,
            message_id: None,
            size: Some(1024),
        }
    }

    #[test]
    fn record_and_check_idempotent() {
        let db = Database::open_memory().unwrap();
        let key = key(100, 42);

        assert!(!db.is_migrated(key).unwrap());

        db.record_migration(MigrationRecord {
            key,
            dst_folder: "INBOX",
            dst_uidvalidity: Some(200),
            dst_uid: Some(7),
            message_id: Some("<m@x>"),
            size: Some(1024),
        })
        .unwrap();
        assert!(db.is_migrated(key).unwrap());
        assert_eq!(db.count_migrated(scope(Some(100))).unwrap(), 1);

        // Idempotent rerun — still one row.
        db.record_migration(MigrationRecord {
            key,
            dst_folder: "INBOX",
            dst_uidvalidity: Some(200),
            dst_uid: Some(7),
            message_id: Some("<m@x>"),
            size: Some(1024),
        })
        .unwrap();
        assert_eq!(db.count_migrated(scope(Some(100))).unwrap(), 1);
    }

    #[test]
    fn migrated_row_with_null_destination_uid_is_still_idempotent() {
        let db = Database::open_memory().unwrap();
        let key = key(100, 42);

        db.record_migration(record(key)).unwrap();

        assert!(db.is_migrated(key).unwrap());
        assert_eq!(db.count_migrated(scope(Some(100))).unwrap(), 1);
    }

    #[test]
    fn message_without_message_id_is_idempotent_by_source_uid_epoch() {
        let db = Database::open_memory().unwrap();
        let key = key(100, 43);

        db.record_migration(MigrationRecord {
            message_id: None,
            ..record(key)
        })
        .unwrap();

        assert!(db.is_migrated(key).unwrap());
    }

    #[test]
    fn account_pair_isolation() {
        let db = Database::open_memory().unwrap();
        db.record_migration(record(key(100, 1))).unwrap();
        // Different dst account — separate row.
        db.record_migration(MigrationRecord {
            key: MigrationKey {
                dst_account_id: "c",
                ..key(100, 1)
            },
            ..record(key(100, 1))
        })
        .unwrap();
        assert_eq!(
            db.count_migrated(MigrationScope {
                src_account_id: "a",
                dst_account_id: "b",
                src_folder: None,
                src_uidvalidity: None,
            })
            .unwrap(),
            1
        );
        assert_eq!(
            db.count_migrated(MigrationScope {
                src_account_id: "a",
                dst_account_id: "c",
                src_folder: None,
                src_uidvalidity: None,
            })
            .unwrap(),
            1
        );
    }

    #[test]
    fn uidvalidity_is_part_of_migration_identity() {
        let db = Database::open_memory().unwrap();
        let old_key = key(100, 42);
        let new_key = key(101, 42);

        db.record_migration(record(old_key)).unwrap();

        assert!(db.is_migrated(old_key).unwrap());
        assert!(!db.is_migrated(new_key).unwrap());

        db.record_migration(record(new_key)).unwrap();
        assert_eq!(db.count_migrated(scope(Some(100))).unwrap(), 1);
        assert_eq!(db.count_migrated(scope(Some(101))).unwrap(), 1);
        assert_eq!(db.count_migrated(scope(None)).unwrap(), 2);
    }

    #[test]
    fn list_migrated_uids_returns_sorted() {
        let db = Database::open_memory().unwrap();
        for uid in [10u32, 3, 7, 1] {
            db.record_migration(record(key(100, uid))).unwrap();
        }
        let uids = db.list_migrated_uids(scope(Some(100))).unwrap();
        assert_eq!(uids, vec![1, 3, 7, 10]);
    }

    #[test]
    fn count_migrated_across_folders() {
        let db = Database::open_memory().unwrap();
        db.record_migration(record(key(100, 1))).unwrap();
        db.record_migration(MigrationRecord {
            key: MigrationKey {
                src_folder: "Sent",
                ..key(100, 1)
            },
            dst_folder: "Sent",
            ..record(key(100, 1))
        })
        .unwrap();
        assert_eq!(db.count_migrated(scope(None)).unwrap(), 1);
        assert_eq!(
            db.count_migrated(MigrationScope {
                src_account_id: "a",
                dst_account_id: "b",
                src_folder: None,
                src_uidvalidity: None,
            })
            .unwrap(),
            2
        );
    }
}
