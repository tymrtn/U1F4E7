// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! What the mail server runs after `envelope rule publish-sieve`, per account.
//!
//! The table is ensured additively outside `PRAGMA user_version`. The V1 and
//! V2 lines share one migration sequence, so a numbered migration here would
//! collide with V2's.

use std::collections::HashSet;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::db::Database;
use crate::errors::Result;

pub(crate) fn ensure_schema(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sieve_publications (
            account_id TEXT PRIMARY KEY,
            host TEXT NOT NULL,
            port INTEGER NOT NULL,
            script_name TEXT NOT NULL,
            script_sha256 TEXT NOT NULL,
            rule_ids TEXT NOT NULL DEFAULT '[]',
            activation TEXT NOT NULL,
            active_script TEXT NOT NULL,
            previous_active_script TEXT,
            server_active INTEGER NOT NULL DEFAULT 1,
            published_at TEXT NOT NULL DEFAULT (datetime('now')),
            checked_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        ",
    )?;
    Ok(())
}

/// The last successful publish for an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SievePublication {
    pub account_id: String,
    pub host: String,
    pub port: u16,
    pub script_name: String,
    pub script_sha256: String,
    /// Ids of the rules the published script contains.
    pub rule_ids: Vec<String>,
    /// What the publish did: `activated`, `kept_wrapper`, `wrapped_existing`
    /// or `replaced_active`.
    pub activation: String,
    /// The script the publish left active: Envelope's script or its wrapper.
    pub active_script: String,
    /// The user's script the wrapper runs first, or the one switched off.
    pub previous_active_script: Option<String>,
    /// False once a later check saw another script active on the server.
    pub server_active: bool,
    pub published_at: String,
    pub checked_at: String,
}

/// A successful publish to record.
#[derive(Debug, Clone)]
pub struct NewSievePublication<'a> {
    pub account_id: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub script_name: &'a str,
    pub script_sha256: &'a str,
    pub rule_ids: &'a [String],
    pub activation: &'a str,
    pub active_script: &'a str,
    pub previous_active_script: Option<&'a str>,
}

impl Database {
    /// Replace the account's record after a successful publish. The server
    /// now runs `rule_ids`.
    pub fn record_sieve_publication(&self, p: &NewSievePublication<'_>) -> Result<()> {
        let rule_ids = serde_json::to_string(p.rule_ids)?;
        self.conn().execute(
            "INSERT OR REPLACE INTO sieve_publications
                (account_id, host, port, script_name, script_sha256, rule_ids,
                 activation, active_script, previous_active_script, server_active,
                 published_at, checked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, datetime('now'), datetime('now'))",
            params![
                p.account_id,
                p.host,
                p.port,
                p.script_name,
                p.script_sha256,
                rule_ids,
                p.activation,
                p.active_script,
                p.previous_active_script,
            ],
        )?;
        Ok(())
    }

    pub fn get_sieve_publication(&self, account_id: &str) -> Result<Option<SievePublication>> {
        let row = self
            .conn()
            .query_row(
                "SELECT account_id, host, port, script_name, script_sha256, rule_ids,
                        activation, active_script, previous_active_script, server_active,
                        published_at, checked_at
                 FROM sieve_publications WHERE account_id = ?1",
                params![account_id],
                |row| {
                    Ok((
                        SievePublication {
                            account_id: row.get(0)?,
                            host: row.get(1)?,
                            port: row.get(2)?,
                            script_name: row.get(3)?,
                            script_sha256: row.get(4)?,
                            rule_ids: Vec::new(),
                            activation: row.get(6)?,
                            active_script: row.get(7)?,
                            previous_active_script: row.get(8)?,
                            server_active: row.get::<_, i64>(9)? != 0,
                            published_at: row.get(10)?,
                            checked_at: row.get(11)?,
                        },
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((mut publication, rule_ids)) = row else {
            return Ok(None);
        };
        publication.rule_ids = serde_json::from_str(&rule_ids)?;
        Ok(Some(publication))
    }

    /// Record what a check of the server saw: whether the script the last
    /// publish left active still is. While it is not, the account's rules
    /// run locally again. Returns false when the account has no record.
    pub fn set_sieve_publication_server_active(
        &self,
        account_id: &str,
        active: bool,
    ) -> Result<bool> {
        let rows = self.conn().execute(
            "UPDATE sieve_publications SET server_active = ?2, checked_at = datetime('now')
             WHERE account_id = ?1",
            params![account_id, active as i32],
        )?;
        Ok(rows > 0)
    }

    /// Ids of the rules the mail server runs for this account: the last
    /// published set, while that script is still the active one.
    pub fn server_managed_rule_ids(&self, account_id: &str) -> Result<HashSet<String>> {
        Ok(match self.get_sieve_publication(account_id)? {
            Some(p) if p.server_active => p.rule_ids.into_iter().collect(),
            _ => HashSet::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish<'a>(rule_ids: &'a [String], sha: &'a str) -> NewSievePublication<'a> {
        NewSievePublication {
            account_id: "acct1",
            host: "sieve.example.test",
            port: 4190,
            script_name: "envelope-rules",
            script_sha256: sha,
            rule_ids,
            activation: "wrapped_existing",
            active_script: "envelope-rules-wrapper",
            previous_active_script: Some("roundcube"),
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_record_means_no_server_managed_rules() {
        let db = Database::open_memory().unwrap();
        assert!(db.get_sieve_publication("acct1").unwrap().is_none());
        assert!(db.server_managed_rule_ids("acct1").unwrap().is_empty());
        assert!(
            !db.set_sieve_publication_server_active("acct1", false)
                .unwrap()
        );
    }

    #[test]
    fn a_publish_records_what_the_server_runs() {
        let db = Database::open_memory().unwrap();
        let rule_ids = ids(&["r1", "r2"]);
        db.record_sieve_publication(&publish(&rule_ids, "abc"))
            .unwrap();

        let got = db.get_sieve_publication("acct1").unwrap().unwrap();
        assert_eq!(got.rule_ids, rule_ids);
        assert_eq!(got.script_sha256, "abc");
        assert_eq!(got.activation, "wrapped_existing");
        assert_eq!(got.active_script, "envelope-rules-wrapper");
        assert_eq!(got.previous_active_script.as_deref(), Some("roundcube"));
        assert!(got.server_active);
        assert_eq!(
            db.server_managed_rule_ids("acct1").unwrap(),
            rule_ids.iter().cloned().collect::<HashSet<_>>()
        );
        assert!(db.server_managed_rule_ids("other").unwrap().is_empty());
    }

    #[test]
    fn republishing_replaces_the_set() {
        let db = Database::open_memory().unwrap();
        db.record_sieve_publication(&publish(&ids(&["r1", "r2"]), "abc"))
            .unwrap();
        db.record_sieve_publication(&publish(&ids(&["r2", "r3"]), "def"))
            .unwrap();

        let managed = db.server_managed_rule_ids("acct1").unwrap();
        assert_eq!(
            managed,
            ["r2", "r3"].iter().map(|s| s.to_string()).collect()
        );
        assert_eq!(
            db.get_sieve_publication("acct1")
                .unwrap()
                .unwrap()
                .script_sha256,
            "def"
        );

        // Publishing a script with no rules hands every rule back.
        db.record_sieve_publication(&publish(&[], "empty")).unwrap();
        assert!(db.server_managed_rule_ids("acct1").unwrap().is_empty());
    }

    #[test]
    fn an_inactive_script_hands_its_rules_back_until_it_is_active_again() {
        let db = Database::open_memory().unwrap();
        db.record_sieve_publication(&publish(&ids(&["r1"]), "abc"))
            .unwrap();

        assert!(
            db.set_sieve_publication_server_active("acct1", false)
                .unwrap()
        );
        assert!(db.server_managed_rule_ids("acct1").unwrap().is_empty());
        assert!(
            !db.get_sieve_publication("acct1")
                .unwrap()
                .unwrap()
                .server_active
        );

        assert!(
            db.set_sieve_publication_server_active("acct1", true)
                .unwrap()
        );
        assert_eq!(db.server_managed_rule_ids("acct1").unwrap().len(), 1);

        // A new publish is active by definition.
        db.set_sieve_publication_server_active("acct1", false)
            .unwrap();
        db.record_sieve_publication(&publish(&ids(&["r1"]), "abc"))
            .unwrap();
        assert!(
            db.get_sieve_publication("acct1")
                .unwrap()
                .unwrap()
                .server_active
        );
    }
}
