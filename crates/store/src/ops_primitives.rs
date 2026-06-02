// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::db::Database;
use crate::errors::Result;
use crate::models::{FailedAuthAttempt, RuleRunAudit, WatchRecord};
use rusqlite::params;
use uuid::Uuid;

pub struct WatchUpsert<'a> {
    pub account_id: &'a str,
    pub folder: &'a str,
    pub status: &'a str,
    pub process_id: Option<i64>,
    pub schedule: Option<&'a str>,
    pub last_heartbeat_at: Option<&'a str>,
    pub last_event_at: Option<&'a str>,
    pub failure_reason: Option<&'a str>,
}

pub struct RuleRunAuditInput<'a> {
    pub account_id: &'a str,
    pub rule_id: Option<&'a str>,
    pub rule_name: Option<&'a str>,
    pub uid: Option<i64>,
    pub folder: Option<&'a str>,
    pub action: Option<&'a str>,
    pub status: &'a str,
    pub error: Option<&'a str>,
}

impl Database {
    pub fn upsert_watch(&self, input: WatchUpsert<'_>) -> Result<WatchRecord> {
        let id = Uuid::new_v4().to_string();
        self.conn().execute(
            "INSERT INTO watch_registry (
                id, account_id, folder, status, process_id, schedule,
                last_heartbeat_at, last_event_at, failure_reason, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))
             ON CONFLICT(account_id, folder) DO UPDATE SET
                status = excluded.status,
                process_id = excluded.process_id,
                schedule = excluded.schedule,
                last_heartbeat_at = COALESCE(excluded.last_heartbeat_at, watch_registry.last_heartbeat_at),
                last_event_at = COALESCE(excluded.last_event_at, watch_registry.last_event_at),
                failure_reason = excluded.failure_reason,
                updated_at = datetime('now')",
            params![
                id,
                input.account_id,
                input.folder,
                input.status,
                input.process_id,
                input.schedule,
                input.last_heartbeat_at,
                input.last_event_at,
                input.failure_reason,
            ],
        )?;
        self.get_watch(input.account_id, input.folder)
    }

    pub fn list_watches(&self, account_id: Option<&str>, limit: u32) -> Result<Vec<WatchRecord>> {
        let sql = if account_id.is_some() {
            "SELECT id, account_id, folder, status, process_id, schedule,
                    last_heartbeat_at, last_event_at, failure_reason, created_at, updated_at
             FROM watch_registry
             WHERE account_id = ?1
             ORDER BY updated_at DESC
             LIMIT ?2"
        } else {
            "SELECT id, account_id, folder, status, process_id, schedule,
                    last_heartbeat_at, last_event_at, failure_reason, created_at, updated_at
             FROM watch_registry
             ORDER BY updated_at DESC
             LIMIT ?1"
        };
        let mut stmt = self.conn().prepare(sql)?;
        let rows = if let Some(id) = account_id {
            stmt.query_map(params![id, limit], map_watch)?
        } else {
            stmt.query_map(params![limit], map_watch)?
        };
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn get_watch(&self, account_id: &str, folder: &str) -> Result<WatchRecord> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, folder, status, process_id, schedule,
                    last_heartbeat_at, last_event_at, failure_reason, created_at, updated_at
             FROM watch_registry
             WHERE account_id = ?1 AND folder = ?2",
        )?;
        Ok(stmt.query_row(params![account_id, folder], map_watch)?)
    }

    pub fn record_failed_auth(
        &self,
        account_id: &str,
        backend: &str,
        reason: &str,
        retry_guidance: Option<&str>,
    ) -> Result<FailedAuthAttempt> {
        let id = Uuid::new_v4().to_string();
        let redacted = redact_auth_reason(reason);
        self.conn().execute(
            "INSERT INTO failed_auth_history (id, account_id, backend, reason, retry_guidance)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, account_id, backend, redacted, retry_guidance],
        )?;
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, backend, reason, retry_guidance, created_at
             FROM failed_auth_history WHERE id = ?1",
        )?;
        Ok(stmt.query_row(params![id], map_failed_auth)?)
    }

    pub fn list_failed_auth(
        &self,
        account_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<FailedAuthAttempt>> {
        let sql = if account_id.is_some() {
            "SELECT id, account_id, backend, reason, retry_guidance, created_at
             FROM failed_auth_history
             WHERE account_id = ?1
             ORDER BY created_at DESC
             LIMIT ?2"
        } else {
            "SELECT id, account_id, backend, reason, retry_guidance, created_at
             FROM failed_auth_history
             ORDER BY created_at DESC
             LIMIT ?1"
        };
        let mut stmt = self.conn().prepare(sql)?;
        let rows = if let Some(id) = account_id {
            stmt.query_map(params![id, limit], map_failed_auth)?
        } else {
            stmt.query_map(params![limit], map_failed_auth)?
        };
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn record_rule_run(&self, input: RuleRunAuditInput<'_>) -> Result<RuleRunAudit> {
        let id = Uuid::new_v4().to_string();
        self.conn().execute(
            "INSERT INTO rule_run_audit (
                id, account_id, rule_id, rule_name, uid, folder, action, status, error
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                input.account_id,
                input.rule_id,
                input.rule_name,
                input.uid,
                input.folder,
                input.action,
                input.status,
                input.error.map(redact_auth_reason),
            ],
        )?;
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, rule_id, rule_name, uid, folder, action, status, error, created_at
             FROM rule_run_audit WHERE id = ?1",
        )?;
        Ok(stmt.query_row(params![id], map_rule_run)?)
    }

    pub fn list_rule_runs(
        &self,
        account_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RuleRunAudit>> {
        let sql = if account_id.is_some() {
            "SELECT id, account_id, rule_id, rule_name, uid, folder, action, status, error, created_at
             FROM rule_run_audit
             WHERE account_id = ?1
             ORDER BY created_at DESC
             LIMIT ?2"
        } else {
            "SELECT id, account_id, rule_id, rule_name, uid, folder, action, status, error, created_at
             FROM rule_run_audit
             ORDER BY created_at DESC
             LIMIT ?1"
        };
        let mut stmt = self.conn().prepare(sql)?;
        let rows = if let Some(id) = account_id {
            stmt.query_map(params![id, limit], map_rule_run)?
        } else {
            stmt.query_map(params![limit], map_rule_run)?
        };
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

fn map_watch(row: &rusqlite::Row<'_>) -> rusqlite::Result<WatchRecord> {
    Ok(WatchRecord {
        id: row.get(0)?,
        account_id: row.get(1)?,
        folder: row.get(2)?,
        status: row.get(3)?,
        process_id: row.get(4)?,
        schedule: row.get(5)?,
        last_heartbeat_at: row.get(6)?,
        last_event_at: row.get(7)?,
        failure_reason: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn map_failed_auth(row: &rusqlite::Row<'_>) -> rusqlite::Result<FailedAuthAttempt> {
    Ok(FailedAuthAttempt {
        id: row.get(0)?,
        account_id: row.get(1)?,
        backend: row.get(2)?,
        reason: row.get(3)?,
        retry_guidance: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn map_rule_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RuleRunAudit> {
    Ok(RuleRunAudit {
        id: row.get(0)?,
        account_id: row.get(1)?,
        rule_id: row.get(2)?,
        rule_name: row.get(3)?,
        uid: row.get(4)?,
        folder: row.get(5)?,
        action: row.get(6)?,
        status: row.get(7)?,
        error: row.get(8)?,
        created_at: row.get(9)?,
    })
}

fn redact_auth_reason(reason: &str) -> String {
    reason
        .split_whitespace()
        .map(|part| {
            let lower = part.to_ascii_lowercase();
            for marker in ["password=", "token=", "secret=", "pass="] {
                if let Some(start) = lower.find(marker) {
                    let value_start = start + marker.len();
                    let mut redacted = part.to_string();
                    redacted.replace_range(value_start.., "[redacted]");
                    return redacted;
                }
            }
            part.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cockpit_primitives_round_trip_and_redact_auth_reason() {
        let db = Database::open_memory().unwrap();
        let watch = db
            .upsert_watch(WatchUpsert {
                account_id: "acc1",
                folder: "INBOX",
                status: "running",
                process_id: Some(99),
                schedule: Some("foreground"),
                last_heartbeat_at: Some("2026-05-09T09:00:00"),
                last_event_at: None,
                failure_reason: None,
            })
            .unwrap();
        assert_eq!(watch.folder, "INBOX");
        assert_eq!(db.list_watches(Some("acc1"), 10).unwrap().len(), 1);
        assert_eq!(db.list_watches(None, 10).unwrap().len(), 1);

        let failed = db
            .record_failed_auth(
                "acc1",
                "smtp",
                "LOGIN failed password=hunter2 token=abc123",
                Some("retry with app password"),
            )
            .unwrap();
        assert!(!failed.reason.contains("hunter2"));
        assert!(!failed.reason.contains("abc123"));
        assert!(failed.reason.contains("[redacted]"));
        assert_eq!(db.list_failed_auth(None, 10).unwrap().len(), 1);

        let run = db
            .record_rule_run(RuleRunAuditInput {
                account_id: "acc1",
                rule_id: Some("rule1"),
                rule_name: Some("Rule One"),
                uid: Some(12),
                folder: Some("INBOX"),
                action: Some("flagged"),
                status: "ok",
                error: None,
            })
            .unwrap();
        assert_eq!(run.rule_name.as_deref(), Some("Rule One"));
        assert_eq!(
            db.list_rule_runs(Some("acc1"), 10).unwrap()[0].uid,
            Some(12)
        );
        assert_eq!(db.list_rule_runs(None, 10).unwrap().len(), 1);
    }
}
