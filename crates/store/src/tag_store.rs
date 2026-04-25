// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::{MessageScore, MessageTag};
use rusqlite::params;

impl Database {
    // ── Tags ────────────────────────────────────────────────────────

    pub fn add_tag(
        &self,
        account_id: &str,
        message_id: &str,
        tag: &str,
        uid: Option<i64>,
        folder: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO message_tags (account_id, message_id, tag, uid, folder)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(account_id, message_id, tag) DO UPDATE SET
                uid = COALESCE(excluded.uid, uid),
                folder = COALESCE(excluded.folder, folder)",
            params![account_id, message_id, tag, uid, folder],
        )?;
        Ok(())
    }

    pub fn remove_tag(&self, account_id: &str, message_id: &str, tag: &str) -> Result<bool> {
        let rows = self.conn().execute(
            "DELETE FROM message_tags WHERE account_id = ?1 AND message_id = ?2 AND tag = ?3",
            params![account_id, message_id, tag],
        )?;
        Ok(rows > 0)
    }

    pub fn get_tags(&self, account_id: &str, message_id: &str) -> Result<Vec<MessageTag>> {
        let mut stmt = self.conn().prepare(
            "SELECT account_id, message_id, tag, uid, folder, created_at
             FROM message_tags WHERE account_id = ?1 AND message_id = ?2",
        )?;
        let rows = stmt.query_map(params![account_id, message_id], |row| {
            Ok(MessageTag {
                account_id: row.get(0)?,
                message_id: row.get(1)?,
                tag: row.get(2)?,
                uid: row.get(3)?,
                folder: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn list_messages_with_tag(&self, account_id: &str, tag: &str) -> Result<Vec<MessageTag>> {
        let mut stmt = self.conn().prepare(
            "SELECT account_id, message_id, tag, uid, folder, created_at
             FROM message_tags WHERE account_id = ?1 AND tag = ?2
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![account_id, tag], |row| {
            Ok(MessageTag {
                account_id: row.get(0)?,
                message_id: row.get(1)?,
                tag: row.get(2)?,
                uid: row.get(3)?,
                folder: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // ── Scores ──────────────────────────────────────────────────────

    pub fn set_score(
        &self,
        account_id: &str,
        message_id: &str,
        dimension: &str,
        value: f64,
        uid: Option<i64>,
        folder: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO message_scores (account_id, message_id, dimension, value, uid, folder, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))
             ON CONFLICT(account_id, message_id, dimension) DO UPDATE SET
                value = excluded.value,
                uid = COALESCE(excluded.uid, uid),
                folder = COALESCE(excluded.folder, folder),
                updated_at = datetime('now')",
            params![account_id, message_id, dimension, value, uid, folder],
        )?;
        Ok(())
    }

    pub fn remove_score(
        &self,
        account_id: &str,
        message_id: &str,
        dimension: &str,
    ) -> Result<bool> {
        let rows = self.conn().execute(
            "DELETE FROM message_scores WHERE account_id = ?1 AND message_id = ?2 AND dimension = ?3",
            params![account_id, message_id, dimension],
        )?;
        Ok(rows > 0)
    }

    pub fn get_scores(&self, account_id: &str, message_id: &str) -> Result<Vec<MessageScore>> {
        let mut stmt = self.conn().prepare(
            "SELECT account_id, message_id, dimension, value, uid, folder, created_at, updated_at
             FROM message_scores WHERE account_id = ?1 AND message_id = ?2",
        )?;
        let rows = stmt.query_map(params![account_id, message_id], |row| {
            Ok(MessageScore {
                account_id: row.get(0)?,
                message_id: row.get(1)?,
                dimension: row.get(2)?,
                value: row.get(3)?,
                uid: row.get(4)?,
                folder: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn list_messages_with_min_score(
        &self,
        account_id: &str,
        dimension: &str,
        min_value: f64,
    ) -> Result<Vec<MessageScore>> {
        let mut stmt = self.conn().prepare(
            "SELECT account_id, message_id, dimension, value, uid, folder, created_at, updated_at
             FROM message_scores
             WHERE account_id = ?1 AND dimension = ?2 AND value >= ?3
             ORDER BY value DESC",
        )?;
        let rows = stmt.query_map(params![account_id, dimension, min_value], |row| {
            Ok(MessageScore {
                account_id: row.get(0)?,
                message_id: row.get(1)?,
                dimension: row.get(2)?,
                value: row.get(3)?,
                uid: row.get(4)?,
                folder: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Database;

    #[test]
    fn tag_roundtrip() {
        let db = Database::open_memory().unwrap();
        db.add_tag("acct1", "<msg@test>", "newsletter", Some(42), Some("INBOX"))
            .unwrap();
        db.add_tag("acct1", "<msg@test>", "automated", Some(42), Some("INBOX"))
            .unwrap();

        let tags = db.get_tags("acct1", "<msg@test>").unwrap();
        assert_eq!(tags.len(), 2);

        let tag_names: Vec<&str> = tags.iter().map(|t| t.tag.as_str()).collect();
        assert!(tag_names.contains(&"newsletter"));
        assert!(tag_names.contains(&"automated"));

        db.remove_tag("acct1", "<msg@test>", "newsletter").unwrap();
        let tags = db.get_tags("acct1", "<msg@test>").unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag, "automated");
    }

    #[test]
    fn score_roundtrip() {
        let db = Database::open_memory().unwrap();
        db.set_score(
            "acct1",
            "<msg@test>",
            "urgent",
            0.9,
            Some(42),
            Some("INBOX"),
        )
        .unwrap();
        db.set_score(
            "acct1",
            "<msg@test>",
            "interesting",
            0.3,
            Some(42),
            Some("INBOX"),
        )
        .unwrap();

        let scores = db.get_scores("acct1", "<msg@test>").unwrap();
        assert_eq!(scores.len(), 2);

        let urgent = scores.iter().find(|s| s.dimension == "urgent").unwrap();
        assert!((urgent.value - 0.9).abs() < f64::EPSILON);

        // Update score
        db.set_score("acct1", "<msg@test>", "urgent", 0.5, None, None)
            .unwrap();
        let scores = db.get_scores("acct1", "<msg@test>").unwrap();
        let urgent = scores.iter().find(|s| s.dimension == "urgent").unwrap();
        assert!((urgent.value - 0.5).abs() < f64::EPSILON);

        db.remove_score("acct1", "<msg@test>", "urgent").unwrap();
        let scores = db.get_scores("acct1", "<msg@test>").unwrap();
        assert_eq!(scores.len(), 1);
    }

    #[test]
    fn list_by_min_score() {
        let db = Database::open_memory().unwrap();
        db.set_score("acct1", "<m1@test>", "urgent", 0.9, None, None)
            .unwrap();
        db.set_score("acct1", "<m2@test>", "urgent", 0.3, None, None)
            .unwrap();
        db.set_score("acct1", "<m3@test>", "urgent", 0.7, None, None)
            .unwrap();

        let high = db
            .list_messages_with_min_score("acct1", "urgent", 0.7)
            .unwrap();
        assert_eq!(high.len(), 2); // m1 (0.9) and m3 (0.7)
    }

    #[test]
    fn list_by_tag() {
        let db = Database::open_memory().unwrap();
        db.add_tag("acct1", "<m1@test>", "newsletter", None, None)
            .unwrap();
        db.add_tag("acct1", "<m2@test>", "newsletter", None, None)
            .unwrap();
        db.add_tag("acct1", "<m3@test>", "vip", None, None).unwrap();

        let newsletters = db.list_messages_with_tag("acct1", "newsletter").unwrap();
        assert_eq!(newsletters.len(), 2);
    }
}
