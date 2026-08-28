// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Aggregate read queries for the v2 Agent Cockpit.
//!
//! These are strictly additive, read-only helpers layered on top of the audit
//! tables (`action_log`, `events`, `drafts`). They power the cockpit's
//! per-agent attribution feed, the aggregate scheduled-send list, and the
//! per-draft Governor verdict surface. Nothing here mutates rows or touches a
//! mailbox — the cockpit invariant is that aggregate loads stay read-only.

use crate::db::Database;
use crate::errors::Result;
use crate::models::Draft;
use rusqlite::{OptionalExtension, params};
use std::collections::HashMap;

/// Per-agent activity counts derived from the additive `agent_id` columns on
/// the audit tables. `agent_id` is the stable agent identity id; both counts
/// are best-effort aggregates over all accounts.
#[derive(Debug, Clone, Default)]
pub struct AgentActivityCounts {
    pub action_count: i64,
    pub event_count: i64,
    /// Most recent `created_at` across this agent's action_log + events rows.
    pub last_activity_at: Option<String>,
}

/// The latest Governor verdict recorded for a single draft, parsed from the
/// sanitized `send_governor.*` audit events. Carries no recipients or bodies.
#[derive(Debug, Clone)]
pub struct GovernorVerdict {
    pub draft_id: String,
    /// `"allow"`, `"review"`, `"deny"`, `"block"`, … (Governor's stable name).
    pub decision: String,
    pub allowed: bool,
    pub block_code: Option<String>,
    pub created_at: String,
}

impl Database {
    /// Count `action_log` + `events` rows grouped by `agent_id`, keyed by the
    /// agent identity id. Rows with a NULL `agent_id` (human / pre-attribution)
    /// are excluded — the cockpit attributes activity to named agents only.
    pub fn agent_activity_counts(&self) -> Result<HashMap<String, AgentActivityCounts>> {
        let mut counts: HashMap<String, AgentActivityCounts> = HashMap::new();

        let mut actions = self.conn().prepare(
            "SELECT agent_id, COUNT(*), MAX(created_at)
             FROM action_log
             WHERE agent_id IS NOT NULL
             GROUP BY agent_id",
        )?;
        let action_rows = actions.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in action_rows {
            let (agent_id, count, last) = row?;
            let entry = counts.entry(agent_id).or_default();
            entry.action_count = count;
            entry.last_activity_at = max_opt(entry.last_activity_at.take(), last);
        }

        let mut events = self.conn().prepare(
            "SELECT agent_id, COUNT(*), MAX(created_at)
             FROM events
             WHERE agent_id IS NOT NULL
             GROUP BY agent_id",
        )?;
        let event_rows = events.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in event_rows {
            let (agent_id, count, last) = row?;
            let entry = counts.entry(agent_id).or_default();
            entry.event_count = count;
            entry.last_activity_at = max_opt(entry.last_activity_at.take(), last);
        }

        Ok(counts)
    }

    /// Drafts across every account filtered to a single status, newest first.
    /// The per-account `list_drafts` cannot span accounts; this is the aggregate
    /// read the cockpit approval queue and scheduled list need.
    pub fn list_all_drafts_by_status(&self, status: &str, limit: u32) -> Result<Vec<Draft>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid, revision
             FROM drafts WHERE status = ?1
             ORDER BY updated_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![status, limit], Self::map_draft)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// True total of drafts in one status across every account — the uncapped
    /// companion to [`Database::list_all_drafts_by_status`], so capped item
    /// lists can still report the whole queue.
    pub fn count_all_drafts_by_status(&self, status: &str) -> Result<i64> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*) FROM drafts WHERE status = ?1",
            params![status],
            |row| row.get(0),
        )?)
    }

    /// True total of scheduled drafts (same predicate as
    /// [`Database::list_scheduled_drafts`]). With `due_before` set, counts only
    /// those whose `send_after` has passed — ISO 8601 strings compare
    /// lexicographically, matching the snooze due queries.
    pub fn count_scheduled_drafts(
        &self,
        account_id: Option<&str>,
        due_before: Option<&str>,
    ) -> Result<i64> {
        let mut sql = String::from(
            "SELECT COUNT(*) FROM drafts
             WHERE status IN ('draft', 'sending', 'syncing') AND send_after IS NOT NULL",
        );
        let mut params_vec: Vec<&str> = Vec::new();
        if let Some(account) = account_id {
            params_vec.push(account);
            sql.push_str(&format!(" AND account_id = ?{}", params_vec.len()));
        }
        if let Some(now) = due_before {
            params_vec.push(now);
            sql.push_str(&format!(" AND send_after <= ?{}", params_vec.len()));
        }
        Ok(self
            .conn()
            .query_row(&sql, rusqlite::params_from_iter(params_vec), |row| {
                row.get(0)
            })?)
    }

    /// Scheduled drafts: those carrying a `send_after` and still queued
    /// (`status = 'draft'`, not yet sent/discarded). Optionally scoped to one
    /// account. Ordered by soonest send time first so countdowns read top-down.
    pub fn list_scheduled_drafts(
        &self,
        account_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Draft>> {
        let base = "SELECT id, account_id, status, to_addr, cc_addr, bcc_addr, reply_to, subject,
                    text_content, html_content, in_reply_to, metadata, attachments, message_id,
                    send_after, snoozed_until, created_at, updated_at, sent_at, created_by,
                    imap_uid, revision
             FROM drafts
             WHERE status IN ('draft', 'sending', 'syncing') AND send_after IS NOT NULL";
        let (sql, scoped) = match account_id {
            Some(_) => (
                format!("{base} AND account_id = ?1 ORDER BY send_after ASC LIMIT ?2"),
                true,
            ),
            None => (format!("{base} ORDER BY send_after ASC LIMIT ?1"), false),
        };
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = if scoped {
            stmt.query_map(params![account_id.unwrap(), limit], Self::map_draft)?
        } else {
            stmt.query_map(params![limit], Self::map_draft)?
        };
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Latest Governor verdict per draft, parsed from the sanitized
    /// `send_governor.allowed` / `send_governor.blocked` audit events. Returns a
    /// map keyed by `draft_id`. Only the newest event per draft wins.
    ///
    /// The payload shape is `{"request": {…, "draft_id": …}, "outcome": {…}}`
    /// (see the dashboard's `run_governor_gate`). Events without a resolvable
    /// draft id are skipped. This never exposes recipients or bodies — the
    /// audit payload is content-free by construction.
    pub fn latest_governor_verdicts(&self, limit: u32) -> Result<HashMap<String, GovernorVerdict>> {
        let mut stmt = self.conn().prepare(
            "SELECT payload, created_at
             FROM events
             WHERE event_type LIKE 'send_governor.%'
             ORDER BY created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut latest: HashMap<String, GovernorVerdict> = HashMap::new();
        for row in rows {
            let (payload_raw, created_at) = row?;
            let Some(payload) = payload_raw
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            else {
                continue;
            };
            let Some(draft_id) = payload
                .get("request")
                .and_then(|r| r.get("draft_id"))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let outcome = payload.get("outcome");
            let decision = outcome
                .and_then(|o| o.get("decision"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let allowed = outcome
                .and_then(|o| o.get("allowed"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let block_code = outcome
                .and_then(|o| o.get("block_code"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // Rows arrive newest-first; only insert if we have not already seen
            // this draft (so the first — newest — verdict wins).
            latest
                .entry(draft_id.to_string())
                .or_insert(GovernorVerdict {
                    draft_id: draft_id.to_string(),
                    decision,
                    allowed,
                    block_code,
                    created_at,
                });
        }
        Ok(latest)
    }

    /// Count of dead-lettered deliveries (retries exhausted). Cheap scalar read
    /// for the watch panel's dead-letter badge.
    pub fn dead_letter_count(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM event_deliveries WHERE dead_lettered_at IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Delivery status counts for a single route: (delivered, pending, dead).
    /// Powers the per-route health summary in the watch panel.
    pub fn route_delivery_counts(&self, route_id: &str) -> Result<(i64, i64, i64)> {
        let mut stmt = self.conn().prepare(
            "SELECT
                COALESCE(SUM(CASE WHEN delivered_at IS NOT NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN delivered_at IS NULL AND dead_lettered_at IS NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN dead_lettered_at IS NOT NULL THEN 1 ELSE 0 END), 0)
             FROM event_deliveries WHERE route_id = ?1",
        )?;
        Ok(stmt.query_row(params![route_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?)
    }
}

/// Return the lexicographically-greater of two optional ISO 8601 timestamps.
/// ISO 8601 sorts lexicographically, so string comparison is a valid ordering.
fn max_opt(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a >= b { a } else { b }),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;

    fn seed_account(db: &Database) {
        db.conn().execute("INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password) VALUES ('acc1', 'Test', 'op@example.com', 'example.com', 'smtp.example.com', 587, 'imap.example.com', 993, 'x')", []).unwrap();
    }

    fn insert_governor_event(
        db: &Database,
        id: &str,
        draft_id: &str,
        decision: &str,
        allowed: bool,
        created_at: &str,
    ) {
        let event_type = if allowed {
            "send_governor.allowed"
        } else {
            "send_governor.blocked"
        };
        let payload = serde_json::json!({
            "request": {"surface": "scheduled", "draft_id": draft_id},
            "outcome": {"allowed": allowed, "decision": decision, "block_code": if allowed { serde_json::Value::Null } else { serde_json::json!("governor_blocked") }},
        });
        db.insert_event(&Event {
            id: id.to_string(),
            account_id: "acc1".to_string(),
            event_type: event_type.to_string(),
            folder: "policy".to_string(),
            uid: None,
            message_id: None,
            from_addr: None,
            subject: None,
            snippet: None,
            payload: Some(payload.to_string()),
            idempotency_key: None,
            secure_pending: false,
            acked_at: Some(created_at.to_string()),
            created_at: created_at.to_string(),
        })
        .unwrap();
    }

    #[test]
    fn agent_activity_counts_group_by_agent_and_ignore_null() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        let agent = db.create_agent("skippy").unwrap();
        // Two events attributed to the agent, one anonymous.
        for (i, aid) in [
            Some(agent.identity.id.as_str()),
            Some(agent.identity.id.as_str()),
            None,
        ]
        .iter()
        .enumerate()
        {
            db.insert_event_with_agent(
                &Event {
                    id: format!("evt-{i}"),
                    account_id: "acc1".to_string(),
                    event_type: "agent.action".to_string(),
                    folder: "INBOX".to_string(),
                    uid: None,
                    message_id: None,
                    from_addr: None,
                    subject: None,
                    snippet: None,
                    payload: None,
                    idempotency_key: None,
                    secure_pending: false,
                    acked_at: None,
                    created_at: format!("2026-07-0{}T00:00:00", i + 1),
                },
                *aid,
            )
            .unwrap();
        }

        let counts = db.agent_activity_counts().unwrap();
        let entry = counts.get(&agent.identity.id).expect("agent has activity");
        assert_eq!(entry.event_count, 2);
        assert_eq!(entry.action_count, 0);
        assert_eq!(
            entry.last_activity_at.as_deref(),
            Some("2026-07-02T00:00:00")
        );
    }

    #[test]
    fn latest_governor_verdict_wins_and_carries_no_recipients() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        insert_governor_event(&db, "g1", "draft-1", "allow", true, "2026-07-01T00:00:00");
        insert_governor_event(&db, "g2", "draft-1", "review", false, "2026-07-02T00:00:00");

        let verdicts = db.latest_governor_verdicts(100).unwrap();
        let v = verdicts.get("draft-1").expect("draft has a verdict");
        assert_eq!(v.decision, "review");
        assert!(!v.allowed);
        assert_eq!(v.block_code.as_deref(), Some("governor_blocked"));
        assert_eq!(v.created_at, "2026-07-02T00:00:00");
    }

    #[test]
    fn scheduled_drafts_only_queued_with_send_after() {
        let db = Database::open_memory().unwrap();
        seed_account(&db);
        let scheduled = db
            .create_draft(
                "acc1",
                "to@example.com",
                Some("Later"),
                Some("body"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_send_after(&scheduled.id, "2030-01-01T00:00:00")
            .unwrap();
        // A plain draft without send_after must not appear.
        db.create_draft(
            "acc1",
            "to@example.com",
            Some("Now"),
            Some("body"),
            None,
            None,
            None,
            None,
            Some("agent"),
        )
        .unwrap();

        let listed = db.list_scheduled_drafts(None, 50).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, scheduled.id);
    }
}
