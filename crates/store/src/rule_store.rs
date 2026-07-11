// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::Rule;
use rusqlite::params;
use uuid::Uuid;

fn is_sieve_exportable(match_expr: &str, action: &str) -> bool {
    const LOCAL_MATCH_KEYS: [&str; 4] =
        ["has_tag", "score_above", "score_below", "contact_has_tag"];
    const LOCAL_ACTION_KEYS: [&str; 4] = ["webhook", "snooze", "unsubscribe", "add_tag"];

    !LOCAL_MATCH_KEYS.iter().any(|key| match_expr.contains(key))
        && !LOCAL_ACTION_KEYS.iter().any(|key| action.contains(key))
}

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
        self.create_rule_with_enabled(account_id, name, match_expr, action, priority, stop, true)
    }

    pub fn create_rule_with_enabled(
        &self,
        account_id: &str,
        name: &str,
        match_expr: &str,
        action: &str,
        priority: i64,
        stop: bool,
        enabled: bool,
    ) -> Result<Rule> {
        let id = Uuid::new_v4().to_string();
        let sieve_exportable = is_sieve_exportable(match_expr, action);

        self.conn().execute(
            "INSERT INTO rules (id, account_id, name, match_expr, action, enabled, priority, stop, sieve_exportable)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                account_id,
                name,
                match_expr,
                action,
                enabled as i32,
                priority,
                stop as i32,
                sieve_exportable as i32,
            ],
        )?;

        self.get_rule(&id)?.ok_or_else(|| {
            crate::errors::StoreError::Config(format!("rule not found after insert: {id}"))
        })
    }

    pub fn get_rule(&self, id: &str) -> Result<Option<Rule>> {
        use rusqlite::OptionalExtension;
        let rule = self
            .conn()
            .query_row(
                "SELECT id, account_id, name, match_expr, action, enabled, priority,
                    stop, sieve_exportable, hit_count, last_hit_at, created_at, updated_at
             FROM rules WHERE id = ?1",
                params![id],
                Self::map_rule,
            )
            .optional()?;
        Ok(rule)
    }

    pub fn find_rule_by_name(&self, account_id: &str, name: &str) -> Result<Option<Rule>> {
        use rusqlite::OptionalExtension;
        let rule = self
            .conn()
            .query_row(
                "SELECT id, account_id, name, match_expr, action, enabled, priority,
                    stop, sieve_exportable, hit_count, last_hit_at, created_at, updated_at
             FROM rules WHERE account_id = ?1 AND name = ?2",
                params![account_id, name],
                Self::map_rule,
            )
            .optional()?;
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
        let rows = self
            .conn()
            .execute("DELETE FROM rules WHERE id = ?1", params![id])?;
        Ok(rows > 0)
    }

    /// Update a rule's name, match expression, action, priority, and stop flag.
    ///
    /// `sieve_exportable` is re-derived from the new match/action pair; the
    /// caller must not pass it explicitly.
    #[allow(clippy::too_many_arguments)]
    pub fn update_rule(
        &self,
        id: &str,
        account_id: &str,
        name: &str,
        match_expr: &str,
        action: &str,
        priority: i64,
        stop: bool,
    ) -> Result<Option<Rule>> {
        let sieve_exportable = is_sieve_exportable(match_expr, action);
        let rows = self.conn().execute(
            "UPDATE rules
             SET name = ?1, match_expr = ?2, action = ?3, priority = ?4,
                 stop = ?5, sieve_exportable = ?6, updated_at = datetime('now')
             WHERE id = ?7 AND account_id = ?8",
            params![
                name,
                match_expr,
                action,
                priority,
                stop as i32,
                sieve_exportable as i32,
                id,
                account_id,
            ],
        )?;
        if rows == 0 {
            return Ok(None);
        }
        self.get_rule(id)
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
        db.create_rule(
            "acct1",
            "test-rule",
            r#"{"from":"*@x"}"#,
            r#"{"move":"Y"}"#,
            100,
            false,
        )
        .unwrap();

        let found = db.find_rule_by_name("acct1", "test-rule").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "test-rule");

        let not_found = db.find_rule_by_name("acct1", "nonexistent").unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn update_rule_modifies_fields_and_re_derives_sieve_exportable() {
        let db = Database::open_memory().unwrap();
        let rule = db
            .create_rule(
                "acct1",
                "Old name",
                r#"{"from":"*@x.com"}"#,
                r#"{"move":"Archive"}"#,
                100,
                false,
            )
            .unwrap();

        // Update to a local-only action — sieve_exportable must become false.
        let updated = db
            .update_rule(
                &rule.id,
                "acct1",
                "New name",
                r#"{"from":"*@y.com"}"#,
                r#"{"webhook":"https://example.com"}"#,
                50,
                true,
            )
            .unwrap()
            .unwrap();

        assert_eq!(updated.name, "New name");
        assert_eq!(updated.priority, 50);
        assert!(updated.stop);
        assert!(!updated.sieve_exportable, "webhook makes rule local-only");

        // Cross-account update returns None (scoped to account_id).
        let cross = db
            .update_rule(
                &rule.id,
                "other-account",
                "X",
                r#"{"from":"a"}"#,
                r#"{"move":"B"}"#,
                1,
                false,
            )
            .unwrap();
        assert!(cross.is_none());
    }

    #[test]
    fn reviewable_rule_can_be_created_disabled() {
        let db = Database::open_memory().unwrap();
        let rule = db
            .create_rule_with_enabled(
                "acct1",
                "Agent suggestion",
                r#"{"from":"*@notifications.example"}"#,
                r#"{"move":"Archive"}"#,
                100,
                false,
                false,
            )
            .unwrap();
        assert!(!rule.enabled);
        assert_eq!(db.list_enabled_rules("acct1").unwrap().len(), 0);
    }

    #[test]
    fn sieve_exportable_false_for_local_only_actions_and_contact_tags() {
        let db = Database::open_memory().unwrap();
        let cases = [
            (
                "contact",
                r#"{"contact_has_tag":"vip"}"#,
                r#"{"move":"VIP"}"#,
            ),
            ("snooze", r#"{"from":"*@x"}"#, r#"{"snooze":"tomorrow"}"#),
            ("unsubscribe", r#"{"from":"*@x"}"#, r#""unsubscribe""#),
            ("tag", r#"{"from":"*@x"}"#, r#"{"add_tag":"processed"}"#),
            (
                "webhook",
                r#"{"from":"*@x"}"#,
                r#"{"webhook":"https://example.com"}"#,
            ),
        ];
        for (name, match_expr, action) in cases {
            let rule = db
                .create_rule("acct1", name, match_expr, action, 100, false)
                .unwrap();
            assert!(!rule.sieve_exportable, "{name} should be local-only");
        }
    }
}
