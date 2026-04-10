// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::Rule;
use rusqlite::params;
use uuid::Uuid;

impl Database {
    pub fn create_rule(
        &self,
        account_id: &str,
        name: &str,
        match_expr: &str,
        action: &str,
        priority: i64,
        stop: bool,
    ) -> Result<Rule> {
        let id = Uuid::new_v4().to_string();

        // Compute sieve_exportable: true if match_expr only uses from/to/subject
        // (no tags, no scores). Simple heuristic: check if the JSON contains
        // "has_tag" or "score_above" or "score_below".
        let sieve_exportable = !match_expr.contains("has_tag")
            && !match_expr.contains("score_above")
            && !match_expr.contains("score_below");

        self.conn().execute(
            "INSERT INTO rules (id, account_id, name, match_expr, action, priority, stop, sieve_exportable)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                account_id,
                name,
                match_expr,
                action,
                priority,
                stop as i32,
                sieve_exportable as i32,
            ],
        )?;

        self.get_rule(&id)?
            .ok_or_else(|| crate::errors::StoreError::Config(format!("rule not found after insert: {id}")))
    }

    pub fn get_rule(&self, id: &str) -> Result<Option<Rule>> {
        use rusqlite::OptionalExtension;
        let rule = self.conn().query_row(
            "SELECT id, account_id, name, match_expr, action, enabled, priority,
                    stop, sieve_exportable, hit_count, last_hit_at, created_at, updated_at
             FROM rules WHERE id = ?1",
            params![id],
            Self::map_rule,
        ).optional()?;
        Ok(rule)
    }

    pub fn find_rule_by_name(&self, account_id: &str, name: &str) -> Result<Option<Rule>> {
        use rusqlite::OptionalExtension;
        let rule = self.conn().query_row(
            "SELECT id, account_id, name, match_expr, action, enabled, priority,
                    stop, sieve_exportable, hit_count, last_hit_at, created_at, updated_at
             FROM rules WHERE account_id = ?1 AND name = ?2",
            params![account_id, name],
            Self::map_rule,
        ).optional()?;
        Ok(rule)
    }

    pub fn list_rules(&self, account_id: &str) -> Result<Vec<Rule>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, name, match_expr, action, enabled, priority,
                    stop, sieve_exportable, hit_count, last_hit_at, created_at, updated_at
             FROM rules WHERE account_id = ?1
             ORDER BY priority ASC, created_at ASC",
        )?;
        let rows = stmt.query_map(params![account_id], Self::map_rule)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn list_enabled_rules(&self, account_id: &str) -> Result<Vec<Rule>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, name, match_expr, action, enabled, priority,
                    stop, sieve_exportable, hit_count, last_hit_at, created_at, updated_at
             FROM rules WHERE account_id = ?1 AND enabled = 1
             ORDER BY priority ASC, created_at ASC",
        )?;
        let rows = stmt.query_map(params![account_id], Self::map_rule)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn enable_rule(&self, id: &str) -> Result<bool> {
        let rows = self.conn().execute(
            "UPDATE rules SET enabled = 1, updated_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        Ok(rows > 0)
    }

    pub fn disable_rule(&self, id: &str) -> Result<bool> {
        let rows = self.conn().execute(
            "UPDATE rules SET enabled = 0, updated_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        Ok(rows > 0)
    }

    pub fn delete_rule(&self, id: &str) -> Result<bool> {
        let rows = self.conn().execute(
            "DELETE FROM rules WHERE id = ?1",
            params![id],
        )?;
        Ok(rows > 0)
    }

    pub fn increment_rule_hit(&self, id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE rules SET hit_count = hit_count + 1, last_hit_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    fn map_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Rule> {
        let enabled_int: i32 = row.get(5)?;
        let stop_int: i32 = row.get(7)?;
        let sieve_int: i32 = row.get(8)?;
        Ok(Rule {
            id: row.get(0)?,
            account_id: row.get(1)?,
            name: row.get(2)?,
            match_expr: row.get(3)?,
            action: row.get(4)?,
            enabled: enabled_int != 0,
            priority: row.get(6)?,
            stop: stop_int != 0,
            sieve_exportable: sieve_int != 0,
            hit_count: row.get(9)?,
            last_hit_at: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Database;

    #[test]
    fn rule_crud() {
        let db = Database::open_memory().unwrap();

        let rule = db
            .create_rule(
                "acct1",
                "GitHub noise",
                r#"{"from":"*@notifications.github.com"}"#,
                r#"{"move":"Archive"}"#,
                100,
                false,
            )
            .unwrap();

        assert_eq!(rule.name, "GitHub noise");
        assert!(rule.enabled);
        assert!(!rule.stop);
        assert!(rule.sieve_exportable); // no tags/scores in match
        assert_eq!(rule.hit_count, 0);

        // List
        let rules = db.list_rules("acct1").unwrap();
        assert_eq!(rules.len(), 1);

        // Disable
        db.disable_rule(&rule.id).unwrap();
        let enabled = db.list_enabled_rules("acct1").unwrap();
        assert_eq!(enabled.len(), 0);

        // Re-enable
        db.enable_rule(&rule.id).unwrap();
        let enabled = db.list_enabled_rules("acct1").unwrap();
        assert_eq!(enabled.len(), 1);

        // Hit count
        db.increment_rule_hit(&rule.id).unwrap();
        db.increment_rule_hit(&rule.id).unwrap();
        let updated = db.get_rule(&rule.id).unwrap().unwrap();
        assert_eq!(updated.hit_count, 2);
        assert!(updated.last_hit_at.is_some());

        // Delete
        db.delete_rule(&rule.id).unwrap();
        assert!(db.get_rule(&rule.id).unwrap().is_none());
    }

    #[test]
    fn rule_sieve_exportable_false_for_tag_match() {
        let db = Database::open_memory().unwrap();
        let rule = db
            .create_rule(
                "acct1",
                "Tag-based rule",
                r#"{"has_tag":"newsletter"}"#,
                r#"{"move":"Junk"}"#,
                100,
                false,
            )
            .unwrap();
        assert!(!rule.sieve_exportable);
    }

    #[test]
    fn find_by_name() {
        let db = Database::open_memory().unwrap();
        db.create_rule("acct1", "test-rule", r#"{"from":"*@x"}"#, r#"{"move":"Y"}"#, 100, false).unwrap();

        let found = db.find_rule_by_name("acct1", "test-rule").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "test-rule");

        let not_found = db.find_rule_by_name("acct1", "nonexistent").unwrap();
        assert!(not_found.is_none());
    }
}
