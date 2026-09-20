// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Durable, privacy-minimized state for the Jev mail engine.
//!
//! The tables are ensured additively outside `PRAGMA user_version`. Envelope's
//! V1 and additive V2 lines already share a version sequence, so reserving a
//! colliding migration number would make either line misinterpret the other.

use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::Database;
use crate::errors::{Result, StoreError};

pub const MAIL_ENGINE_SCHEMA_VERSION: u32 = 1;
pub const MAX_DECISION_JSON_BYTES: usize = 64 * 1024;

pub(crate) fn ensure_schema(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS mail_engine_mailboxes (
            account_id TEXT NOT NULL,
            folder TEXT NOT NULL,
            uidvalidity INTEGER NOT NULL,
            last_seen_uid INTEGER NOT NULL DEFAULT 0,
            baseline_reason TEXT NOT NULL,
            last_error TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (account_id, folder)
        );

        CREATE TABLE IF NOT EXISTS mail_engine_decisions (
            account_id TEXT NOT NULL,
            folder TEXT NOT NULL,
            uidvalidity INTEGER NOT NULL,
            uid INTEGER NOT NULL,
            input_hash TEXT NOT NULL,
            model TEXT NOT NULL,
            status TEXT NOT NULL,
            route TEXT NOT NULL,
            route_probability REAL,
            route_confidence REAL,
            urgency TEXT NOT NULL,
            notify_user_probability REAL,
            requires_reply_probability REAL,
            bulk_or_subscription_probability REAL,
            decision_json TEXT NOT NULL DEFAULT '{}',
            execution_status TEXT NOT NULL DEFAULT 'not_requested',
            executed_action TEXT,
            last_error TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (account_id, folder, uidvalidity, uid)
        );
        CREATE INDEX IF NOT EXISTS idx_mail_engine_decisions_route
            ON mail_engine_decisions(account_id, route, created_at);
        CREATE INDEX IF NOT EXISTS idx_mail_engine_decisions_status
            ON mail_engine_decisions(account_id, status, created_at);

        CREATE TABLE IF NOT EXISTS mail_engine_digest_consumptions (
            account_id TEXT NOT NULL,
            folder TEXT NOT NULL,
            uidvalidity INTEGER NOT NULL,
            uid INTEGER NOT NULL,
            consumed_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (account_id, folder, uidvalidity, uid)
        );

        CREATE TABLE IF NOT EXISTS mail_engine_decision_corrections (
            account_id TEXT NOT NULL,
            folder TEXT NOT NULL,
            uidvalidity INTEGER NOT NULL,
            uid INTEGER NOT NULL,
            revision INTEGER NOT NULL,
            route TEXT NOT NULL,
            urgency TEXT NOT NULL,
            actor TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (account_id, folder, uidvalidity, uid, revision)
        );

        CREATE TABLE IF NOT EXISTS mail_engine_sender_stats (
            account_id TEXT NOT NULL,
            sender_hash TEXT NOT NULL,
            domain_hash TEXT NOT NULL,
            total_received INTEGER NOT NULL DEFAULT 0,
            read_count INTEGER NOT NULL DEFAULT 0,
            unread_count INTEGER NOT NULL DEFAULT 0,
            junk_count INTEGER NOT NULL DEFAULT 0,
            replied_thread_count INTEGER NOT NULL DEFAULT 0,
            outbound_count INTEGER NOT NULL DEFAULT 0,
            inbound_count INTEGER NOT NULL DEFAULT 0,
            distinct_thread_count INTEGER NOT NULL DEFAULT 0,
            first_seen TEXT,
            last_seen TEXT,
            history_complete INTEGER NOT NULL DEFAULT 0,
            source_version INTEGER NOT NULL DEFAULT 1,
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (account_id, sender_hash)
        );
        CREATE INDEX IF NOT EXISTS idx_mail_engine_sender_domain
            ON mail_engine_sender_stats(account_id, domain_hash);
        ",
    )?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MailboxScanPlan {
    Baseline {
        uidvalidity: u32,
        last_seen_uid: u32,
    },
    Rebaseline {
        uidvalidity: u32,
        last_seen_uid: u32,
        reason: String,
    },
    Current,
    NewRange {
        uidvalidity: u32,
        after_uid: u32,
        through_uid: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct MailEngineSenderStats {
    pub total_received: u64,
    pub read_count: u64,
    pub unread_count: u64,
    pub junk_count: u64,
    pub replied_thread_count: u64,
    pub outbound_count: u64,
    pub inbound_count: u64,
    pub distinct_thread_count: u64,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub history_complete: bool,
    pub source_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NewMailEngineDecision<'a> {
    pub account_id: &'a str,
    pub folder: &'a str,
    pub uidvalidity: u32,
    pub uid: u32,
    pub input_hash: &'a str,
    pub model: &'a str,
    pub status: &'a str,
    pub route: &'a str,
    pub route_probability: Option<f64>,
    pub route_confidence: Option<f64>,
    pub urgency: &'a str,
    pub notify_user_probability: Option<f64>,
    pub requires_reply_probability: Option<f64>,
    pub bulk_or_subscription_probability: Option<f64>,
    /// Typed answer data only. Never place message/sender content here.
    pub decision_json: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MailEngineDecisionClaim<'a> {
    pub account_id: &'a str,
    pub folder: &'a str,
    pub uidvalidity: u32,
    pub uid: u32,
    pub input_hash: &'a str,
    pub model: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MailEngineStatus {
    pub account_id: String,
    pub folder: String,
    pub uidvalidity: u32,
    pub last_seen_uid: u32,
    pub baseline_reason: String,
    pub decision_count: u64,
    pub review_count: u64,
    pub processing_count: u64,
    pub junk_count: u64,
    pub follow_up_count: u64,
    pub important_count: u64,
    pub routine_count: u64,
    pub digest_news_count: u64,
    pub unsubscribe_candidate_count: u64,
    pub notification_count: u64,
    pub last_error: Option<String>,
}

/// Privacy-minimized decision record for operator inspection. Message headers
/// and bodies stay in IMAP and are fetched only by an explicit review command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MailEngineDecisionSummary {
    pub account_id: String,
    pub folder: String,
    pub uidvalidity: u32,
    pub uid: u32,
    pub model_status: String,
    pub status: String,
    pub route: String,
    pub effective_route: String,
    pub route_probability: Option<f64>,
    pub route_confidence: Option<f64>,
    pub urgency: String,
    pub effective_urgency: String,
    pub notify_user_probability: Option<f64>,
    pub requires_reply_probability: Option<f64>,
    pub bulk_or_subscription_probability: Option<f64>,
    pub execution_status: String,
    pub executed_action: Option<String>,
    pub model_error_code: Option<String>,
    pub error_code: Option<String>,
    pub correction_revision: u64,
    pub decided_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MailEngineDecisionDisplay {
    #[serde(flatten)]
    pub decision: MailEngineDecisionSummary,
    pub metadata_state: String,
    pub from_addr: Option<String>,
    pub subject: Option<String>,
    pub date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MailEngineDecisionRecovery {
    pub status: String,
    pub route: String,
    pub execution_status: String,
    pub urgency: String,
    pub notify_user_probability: Option<f64>,
}

/// Privacy-minimized handle for a message routed into the digest queue.
/// Message content remains in the mailbox and can be fetched read-only by UID
/// when a digest compiler is ready to render a batch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MailEngineDigestCandidate {
    pub account_id: String,
    pub folder: String,
    pub uidvalidity: u32,
    pub uid: u32,
    pub route_probability: Option<f64>,
    pub route_confidence: Option<f64>,
    pub urgency: String,
    pub requires_reply_probability: Option<f64>,
    pub bulk_or_subscription_probability: Option<f64>,
    pub decided_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct MailEngineDigestKey {
    pub account_id: String,
    pub folder: String,
    pub uidvalidity: u32,
    pub uid: u32,
}

impl Database {
    /// Plan a new-only mailbox scan. The first observation and every
    /// UIDVALIDITY change establish a baseline at the current highest UID and
    /// never replay historical messages.
    pub fn plan_mail_engine_scan(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u32,
        highest_uid: u32,
    ) -> Result<MailboxScanPlan> {
        let prior: Option<(i64, i64)> = self
            .conn()
            .query_row(
                "SELECT uidvalidity, last_seen_uid FROM mail_engine_mailboxes
                 WHERE account_id = ?1 AND folder = ?2",
                params![account_id, folder],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        match prior {
            None => {
                self.conn().execute(
                    "INSERT INTO mail_engine_mailboxes
                     (account_id, folder, uidvalidity, last_seen_uid, baseline_reason)
                     VALUES (?1, ?2, ?3, ?4, 'first_observation')",
                    params![
                        account_id,
                        folder,
                        i64::from(uidvalidity),
                        i64::from(highest_uid)
                    ],
                )?;
                Ok(MailboxScanPlan::Baseline {
                    uidvalidity,
                    last_seen_uid: highest_uid,
                })
            }
            Some((old_validity, _)) if old_validity != i64::from(uidvalidity) => {
                let reason = "uidvalidity_changed".to_string();
                self.conn().execute(
                    "UPDATE mail_engine_mailboxes
                     SET uidvalidity = ?3, last_seen_uid = ?4,
                         baseline_reason = ?5, last_error = NULL,
                         updated_at = datetime('now')
                     WHERE account_id = ?1 AND folder = ?2",
                    params![
                        account_id,
                        folder,
                        i64::from(uidvalidity),
                        i64::from(highest_uid),
                        reason,
                    ],
                )?;
                Ok(MailboxScanPlan::Rebaseline {
                    uidvalidity,
                    last_seen_uid: highest_uid,
                    reason,
                })
            }
            Some((_, last_seen)) => {
                let last_seen = last_seen as u32;
                if highest_uid <= last_seen {
                    Ok(MailboxScanPlan::Current)
                } else {
                    Ok(MailboxScanPlan::NewRange {
                        uidvalidity,
                        after_uid: last_seen,
                        through_uid: highest_uid,
                    })
                }
            }
        }
    }

    /// Advance only after every UID up to `uid` has a durable terminal decision
    /// or review state. A stale UIDVALIDITY can never advance a reset mailbox.
    pub fn advance_mail_engine_watermark(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u32,
        uid: u32,
    ) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE mail_engine_mailboxes
             SET last_seen_uid = ?4, updated_at = datetime('now'), last_error = NULL
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3
               AND last_seen_uid < ?4",
            params![account_id, folder, i64::from(uidvalidity), i64::from(uid)],
        )?;
        Ok(changed == 1)
    }

    pub fn record_mail_engine_error(
        &self,
        account_id: &str,
        folder: &str,
        error_code: &str,
    ) -> Result<()> {
        validate_token(error_code, "error_code")?;
        self.conn().execute(
            "UPDATE mail_engine_mailboxes
             SET last_error = ?3, updated_at = datetime('now')
             WHERE account_id = ?1 AND folder = ?2",
            params![account_id, folder, error_code],
        )?;
        Ok(())
    }

    /// Atomically claim one message before any paid model request. A competing
    /// worker or crash retry sees the existing row and must not call Jev again.
    pub fn claim_mail_engine_decision(&self, claim: &MailEngineDecisionClaim<'_>) -> Result<bool> {
        let changed = self.conn().execute(
            "INSERT OR IGNORE INTO mail_engine_decisions (
                account_id, folder, uidvalidity, uid, input_hash, model,
                status, route, urgency, decision_json, execution_status
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                       'processing', 'review', 'not_urgent',
                       '{\"status\":\"processing\"}', 'not_requested')",
            params![
                claim.account_id,
                claim.folder,
                i64::from(claim.uidvalidity),
                i64::from(claim.uid),
                claim.input_hash,
                claim.model,
            ],
        )?;
        Ok(changed == 1)
    }

    /// Insert one immutable model decision. Returns false when a crash retry
    /// finds the same message already decided, preventing duplicate model spend.
    pub fn insert_mail_engine_decision_if_absent(
        &self,
        decision: &NewMailEngineDecision<'_>,
    ) -> Result<bool> {
        for (value, field) in [
            (decision.status, "status"),
            (decision.route, "route"),
            (decision.urgency, "urgency"),
        ] {
            validate_token(value, field)?;
        }
        if decision.decision_json.len() > MAX_DECISION_JSON_BYTES {
            return Err(StoreError::Config(format!(
                "mail engine decision_json exceeds {MAX_DECISION_JSON_BYTES} bytes"
            )));
        }
        let parsed: serde_json::Value = serde_json::from_str(decision.decision_json)?;
        if !parsed.is_object() {
            return Err(StoreError::Config(
                "mail engine decision_json must be an object".into(),
            ));
        }
        for (value, field) in [
            (decision.route_probability, "route_probability"),
            (decision.route_confidence, "route_confidence"),
            (decision.notify_user_probability, "notify_user_probability"),
            (
                decision.requires_reply_probability,
                "requires_reply_probability",
            ),
            (
                decision.bulk_or_subscription_probability,
                "bulk_or_subscription_probability",
            ),
        ] {
            validate_optional_probability(value, field)?;
        }
        let changed = self.conn().execute(
            "INSERT OR IGNORE INTO mail_engine_decisions (
                account_id, folder, uidvalidity, uid, input_hash, model,
                status, route, route_probability, route_confidence, urgency,
                notify_user_probability, requires_reply_probability,
                bulk_or_subscription_probability, decision_json, execution_status
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                       CASE WHEN ?8 = 'junk' THEN 'pending' ELSE 'not_requested' END)",
            params![
                decision.account_id,
                decision.folder,
                i64::from(decision.uidvalidity),
                i64::from(decision.uid),
                decision.input_hash,
                decision.model,
                decision.status,
                decision.route,
                decision.route_probability,
                decision.route_confidence,
                decision.urgency,
                decision.notify_user_probability,
                decision.requires_reply_probability,
                decision.bulk_or_subscription_probability,
                decision.decision_json,
            ],
        )?;
        Ok(changed == 1)
    }

    /// Finalize an existing pre-request claim. The immutable input hash must
    /// match, and only a `processing` row can transition to a terminal result.
    pub fn finalize_mail_engine_decision(
        &self,
        decision: &NewMailEngineDecision<'_>,
    ) -> Result<bool> {
        validate_decision_for_storage(decision)?;
        let changed = self.conn().execute(
            "UPDATE mail_engine_decisions SET
                model = ?6, status = ?7, route = ?8,
                route_probability = ?9, route_confidence = ?10,
                urgency = ?11, notify_user_probability = ?12,
                requires_reply_probability = ?13,
                bulk_or_subscription_probability = ?14,
                decision_json = ?15,
                execution_status = CASE WHEN ?8 = 'junk' THEN 'pending' ELSE 'not_requested' END,
                updated_at = datetime('now')
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4
               AND input_hash = ?5 AND status = 'processing'",
            params![
                decision.account_id,
                decision.folder,
                i64::from(decision.uidvalidity),
                i64::from(decision.uid),
                decision.input_hash,
                decision.model,
                decision.status,
                decision.route,
                decision.route_probability,
                decision.route_confidence,
                decision.urgency,
                decision.notify_user_probability,
                decision.requires_reply_probability,
                decision.bulk_or_subscription_probability,
                decision.decision_json,
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn get_mail_engine_decision_execution(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u32,
        uid: u32,
    ) -> Result<Option<MailEngineDecisionRecovery>> {
        self.conn()
            .query_row(
                "SELECT
                    CASE WHEN c.revision IS NOT NULL THEN 'decided' ELSE d.status END,
                    COALESCE(c.route, d.route),
                    d.execution_status,
                    COALESCE(c.urgency, d.urgency),
                    d.notify_user_probability
                 FROM mail_engine_decisions d
                 LEFT JOIN mail_engine_decision_corrections c
                   ON c.account_id = d.account_id AND c.folder = d.folder
                  AND c.uidvalidity = d.uidvalidity AND c.uid = d.uid
                  AND c.revision = (
                      SELECT MAX(c2.revision)
                      FROM mail_engine_decision_corrections c2
                      WHERE c2.account_id = d.account_id AND c2.folder = d.folder
                        AND c2.uidvalidity = d.uidvalidity AND c2.uid = d.uid
                  )
                 WHERE d.account_id = ?1 AND d.folder = ?2
                   AND d.uidvalidity = ?3 AND d.uid = ?4",
                params![account_id, folder, i64::from(uidvalidity), i64::from(uid)],
                |row| {
                    Ok(MailEngineDecisionRecovery {
                        status: row.get(0)?,
                        route: row.get(1)?,
                        execution_status: row.get(2)?,
                        urgency: row.get(3)?,
                        notify_user_probability: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Resolve an abandoned current-epoch claim without issuing another paid
    /// request. The message remains explicitly queued for human review and a
    /// later ordered pass may advance past it.
    pub fn resolve_current_mail_engine_processing_as_review(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE mail_engine_decisions
             SET status = 'review', route = 'review', urgency = 'not_urgent',
                 route_probability = NULL, route_confidence = NULL,
                 notify_user_probability = NULL,
                 requires_reply_probability = NULL,
                 bulk_or_subscription_probability = NULL,
                 decision_json = '{\"error_code\":\"processing_recovered_for_review\"}',
                 execution_status = 'not_requested', executed_action = NULL,
                 last_error = 'processing_recovered_for_review',
                 updated_at = datetime('now')
             WHERE account_id = ?1 AND folder = ?2 AND uid = ?3
               AND status = 'processing'
               AND uidvalidity = (
                   SELECT uidvalidity FROM mail_engine_mailboxes
                   WHERE account_id = ?1 AND folder = ?2
               )",
            params![account_id, folder, i64::from(uid)],
        )?;
        Ok(changed == 1)
    }

    /// Park stale pre-request claims for human review without issuing another
    /// paid request. A late worker cannot finalize because finalization requires
    /// status='processing'.
    pub fn recover_stale_mail_engine_processing(&self, stale_after_seconds: u64) -> Result<usize> {
        if !(60..=86_400).contains(&stale_after_seconds) {
            return Err(StoreError::Config(
                "mail engine stale-claim threshold must be between 60 and 86400 seconds".into(),
            ));
        }
        let cutoff = format!("-{stale_after_seconds} seconds");
        let changed = self.conn().execute(
            "UPDATE mail_engine_decisions
             SET status = 'review', route = 'review', urgency = 'not_urgent',
                 route_probability = NULL, route_confidence = NULL,
                 notify_user_probability = NULL,
                 requires_reply_probability = NULL,
                 bulk_or_subscription_probability = NULL,
                 decision_json = '{\"error_code\":\"decision_interrupted\"}',
                 execution_status = 'not_requested', executed_action = NULL,
                 last_error = 'decision_interrupted', updated_at = datetime('now')
             WHERE status = 'processing'
               AND updated_at <= datetime('now', ?1)",
            [cutoff],
        )?;
        Ok(changed)
    }

    /// Prepare a retry only when the stored failure proves that no model
    /// request was dispatched. Atomically transitions the existing durable row
    /// back to `processing`; it never deletes the decision or rewinds the mailbox
    /// watermark. The caller must immediately fetch and reclassify this exact
    /// identity using the already-owned claim.
    pub fn prepare_current_mail_engine_safe_retry(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<Option<u32>> {
        let tx = self.conn().unchecked_transaction()?;
        let uidvalidity: Option<i64> = tx
            .query_row(
                "SELECT d.uidvalidity
                 FROM mail_engine_decisions d
                 JOIN mail_engine_mailboxes m
                   ON m.account_id = d.account_id AND m.folder = d.folder
                  AND m.uidvalidity = d.uidvalidity
                 WHERE d.account_id = ?1 AND d.folder = ?2 AND d.uid = ?3
                   AND d.status = 'review' AND d.route = 'review'
                   AND json_extract(d.decision_json, '$.error_code') = 'openrouter_api_key_missing'
                   AND NOT EXISTS (
                       SELECT 1 FROM mail_engine_decision_corrections c
                       WHERE c.account_id = d.account_id AND c.folder = d.folder
                         AND c.uidvalidity = d.uidvalidity AND c.uid = d.uid
                   )",
                params![account_id, folder, i64::from(uid)],
                |row| row.get(0),
            )
            .optional()?;
        let Some(uidvalidity) = uidvalidity else {
            tx.commit()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE mail_engine_decisions
             SET status = 'processing', route = 'review', urgency = 'not_urgent',
                 route_probability = NULL, route_confidence = NULL,
                 notify_user_probability = NULL,
                 requires_reply_probability = NULL,
                 bulk_or_subscription_probability = NULL,
                 decision_json = '{\"status\":\"processing\",\"retry\":\"openrouter_key_restored\"}',
                 execution_status = 'not_requested', executed_action = NULL,
                 last_error = NULL, updated_at = datetime('now')
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4
               AND status = 'review' AND route = 'review'
               AND json_extract(decision_json, '$.error_code') = 'openrouter_api_key_missing'
               AND NOT EXISTS (
                   SELECT 1 FROM mail_engine_decision_corrections c
                   WHERE c.account_id = mail_engine_decisions.account_id
                     AND c.folder = mail_engine_decisions.folder
                     AND c.uidvalidity = mail_engine_decisions.uidvalidity
                     AND c.uid = mail_engine_decisions.uid
               )",
            params![account_id, folder, uidvalidity, i64::from(uid)],
        )?;
        tx.commit()?;
        Ok((changed == 1).then_some(uidvalidity as u32))
    }

    pub fn claim_mail_engine_execution(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u32,
        uid: u32,
    ) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE mail_engine_decisions
             SET execution_status = 'executing', updated_at = datetime('now')
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4
               AND execution_status = 'pending'
               AND COALESCE(
                   (
                       SELECT c.route
                       FROM mail_engine_decision_corrections c
                       WHERE c.account_id = mail_engine_decisions.account_id
                         AND c.folder = mail_engine_decisions.folder
                         AND c.uidvalidity = mail_engine_decisions.uidvalidity
                         AND c.uid = mail_engine_decisions.uid
                       ORDER BY c.revision DESC
                       LIMIT 1
                   ),
                   route
               ) = 'junk'",
            params![account_id, folder, i64::from(uidvalidity), i64::from(uid)],
        )?;
        Ok(changed == 1)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_mail_engine_execution(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u32,
        uid: u32,
        execution_status: &str,
        executed_action: Option<&str>,
        error_code: Option<&str>,
    ) -> Result<bool> {
        validate_token(execution_status, "execution_status")?;
        if let Some(action) = executed_action {
            validate_token(action, "executed_action")?;
        }
        if let Some(error) = error_code {
            validate_token(error, "error_code")?;
        }
        let changed = self.conn().execute(
            "UPDATE mail_engine_decisions
             SET execution_status = ?5, executed_action = ?6, last_error = ?7,
                 updated_at = datetime('now')
             WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4",
            params![
                account_id,
                folder,
                i64::from(uidvalidity),
                i64::from(uid),
                execution_status,
                executed_action,
                error_code,
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn list_pending_mail_engine_junk(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u32,
        limit: usize,
    ) -> Result<Vec<u32>> {
        let mut stmt = self.conn().prepare(
            "SELECT d.uid FROM mail_engine_decisions d
             LEFT JOIN mail_engine_decision_corrections c
               ON c.account_id = d.account_id AND c.folder = d.folder
              AND c.uidvalidity = d.uidvalidity AND c.uid = d.uid
              AND c.revision = (
                  SELECT MAX(c2.revision) FROM mail_engine_decision_corrections c2
                  WHERE c2.account_id = d.account_id AND c2.folder = d.folder
                    AND c2.uidvalidity = d.uidvalidity AND c2.uid = d.uid
              )
             WHERE d.account_id = ?1 AND d.folder = ?2 AND d.uidvalidity = ?3
               AND COALESCE(c.route, d.route) = 'junk'
               AND d.execution_status = 'pending'
             ORDER BY d.uid ASC LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![account_id, folder, i64::from(uidvalidity), limit as i64],
            |row| Ok(row.get::<_, i64>(0)? as u32),
        )?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    pub fn list_mail_engine_digest_queue(
        &self,
        account_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MailEngineDigestCandidate>> {
        if !(1..=100).contains(&limit) {
            return Err(StoreError::Config(
                "mail engine digest queue limit must be between 1 and 100".into(),
            ));
        }
        let mut stmt = self.conn().prepare(
            "SELECT d.account_id, d.folder, d.uidvalidity, d.uid,
                    d.route_probability, d.route_confidence, COALESCE(r.urgency, d.urgency),
                    d.requires_reply_probability, d.bulk_or_subscription_probability,
                    d.updated_at
             FROM mail_engine_decisions d
             JOIN mail_engine_mailboxes m
               ON m.account_id = d.account_id AND m.folder = d.folder
              AND m.uidvalidity = d.uidvalidity
             LEFT JOIN mail_engine_digest_consumptions c
               ON c.account_id = d.account_id AND c.folder = d.folder
              AND c.uidvalidity = d.uidvalidity AND c.uid = d.uid
             LEFT JOIN mail_engine_decision_corrections r
               ON r.account_id = d.account_id AND r.folder = d.folder
              AND r.uidvalidity = d.uidvalidity AND r.uid = d.uid
              AND r.revision = (
                  SELECT MAX(r2.revision) FROM mail_engine_decision_corrections r2
                  WHERE r2.account_id = d.account_id AND r2.folder = d.folder
                    AND r2.uidvalidity = d.uidvalidity AND r2.uid = d.uid
              )
             WHERE COALESCE(r.route, d.route) = 'digest_news' AND d.status = 'decided'
               AND c.uid IS NULL
               AND (?1 IS NULL OR d.account_id = ?1)
             ORDER BY d.updated_at DESC, d.account_id ASC, d.folder ASC,
                      d.uidvalidity DESC, d.uid DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![account_id, limit as i64], |row| {
            Ok(MailEngineDigestCandidate {
                account_id: row.get(0)?,
                folder: row.get(1)?,
                uidvalidity: row.get::<_, i64>(2)? as u32,
                uid: row.get::<_, i64>(3)? as u32,
                route_probability: row.get(4)?,
                route_confidence: row.get(5)?,
                urgency: row.get(6)?,
                requires_reply_probability: row.get(7)?,
                bulk_or_subscription_probability: row.get(8)?,
                decided_at: row.get(9)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn consume_mail_engine_digest(&self, keys: &[MailEngineDigestKey]) -> Result<usize> {
        if keys.len() > 100 {
            return Err(StoreError::Config(
                "mail engine digest consumption is limited to 100 items".into(),
            ));
        }
        let unique = keys.iter().collect::<HashSet<_>>();
        let tx = self.conn().unchecked_transaction()?;
        let mut consumed = 0usize;
        for key in unique {
            consumed += tx.execute(
                "INSERT OR IGNORE INTO mail_engine_digest_consumptions
                 (account_id, folder, uidvalidity, uid)
                 SELECT d.account_id, d.folder, d.uidvalidity, d.uid
                 FROM mail_engine_decisions d
                 JOIN mail_engine_mailboxes m
                   ON m.account_id = d.account_id AND m.folder = d.folder
                  AND m.uidvalidity = d.uidvalidity
                 LEFT JOIN mail_engine_decision_corrections r
                   ON r.account_id = d.account_id AND r.folder = d.folder
                  AND r.uidvalidity = d.uidvalidity AND r.uid = d.uid
                  AND r.revision = (
                      SELECT MAX(r2.revision) FROM mail_engine_decision_corrections r2
                      WHERE r2.account_id = d.account_id AND r2.folder = d.folder
                        AND r2.uidvalidity = d.uidvalidity AND r2.uid = d.uid
                  )
                 WHERE d.account_id = ?1 AND d.folder = ?2
                   AND d.uidvalidity = ?3 AND d.uid = ?4
                   AND d.status = 'decided'
                   AND COALESCE(r.route, d.route) = 'digest_news'",
                params![
                    key.account_id,
                    key.folder,
                    i64::from(key.uidvalidity),
                    i64::from(key.uid),
                ],
            )?;
        }
        tx.commit()?;
        Ok(consumed)
    }

    pub fn count_pending_mail_engine_digest(&self, account_id: Option<&str>) -> Result<usize> {
        let count = self.conn().query_row(
            "SELECT COUNT(*)
             FROM mail_engine_decisions d
             JOIN mail_engine_mailboxes m
               ON m.account_id = d.account_id AND m.folder = d.folder
              AND m.uidvalidity = d.uidvalidity
             LEFT JOIN mail_engine_digest_consumptions c
               ON c.account_id = d.account_id AND c.folder = d.folder
              AND c.uidvalidity = d.uidvalidity AND c.uid = d.uid
             LEFT JOIN mail_engine_decision_corrections r
               ON r.account_id = d.account_id AND r.folder = d.folder
              AND r.uidvalidity = d.uidvalidity AND r.uid = d.uid
              AND r.revision = (
                  SELECT MAX(r2.revision) FROM mail_engine_decision_corrections r2
                  WHERE r2.account_id = d.account_id AND r2.folder = d.folder
                    AND r2.uidvalidity = d.uidvalidity AND r2.uid = d.uid
              )
             WHERE COALESCE(r.route, d.route) = 'digest_news' AND d.status = 'decided'
               AND c.uid IS NULL
               AND (?1 IS NULL OR d.account_id = ?1)",
            [account_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(count as usize)
    }

    pub fn list_mail_engine_decisions(
        &self,
        account_id: Option<&str>,
        route: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MailEngineDecisionSummary>> {
        if !(1..=200).contains(&limit) {
            return Err(StoreError::Config(
                "mail engine decision limit must be between 1 and 200".into(),
            ));
        }
        if let Some(route) = route {
            validate_token(route, "route")?;
        }
        if let Some(status) = status {
            validate_token(status, "status")?;
        }
        let mut stmt = self.conn().prepare(
            "SELECT d.account_id, d.folder, d.uidvalidity, d.uid,
                    d.status,
                    CASE WHEN c.revision IS NOT NULL THEN 'decided' ELSE d.status END,
                    d.route, COALESCE(c.route, d.route),
                    d.route_probability, d.route_confidence,
                    d.urgency, COALESCE(c.urgency, d.urgency),
                    d.notify_user_probability,
                    d.requires_reply_probability, d.bulk_or_subscription_probability,
                    d.execution_status, d.executed_action,
                    COALESCE(d.last_error, json_extract(d.decision_json, '$.error_code')),
                    CASE WHEN c.revision IS NULL
                         THEN COALESCE(d.last_error, json_extract(d.decision_json, '$.error_code'))
                         ELSE NULL END,
                    COALESCE(c.revision, 0), d.updated_at
             FROM mail_engine_decisions d
             JOIN mail_engine_mailboxes m
               ON m.account_id = d.account_id AND m.folder = d.folder
              AND m.uidvalidity = d.uidvalidity
             LEFT JOIN mail_engine_decision_corrections c
               ON c.account_id = d.account_id AND c.folder = d.folder
              AND c.uidvalidity = d.uidvalidity AND c.uid = d.uid
              AND c.revision = (
                  SELECT MAX(c2.revision)
                  FROM mail_engine_decision_corrections c2
                  WHERE c2.account_id = d.account_id AND c2.folder = d.folder
                    AND c2.uidvalidity = d.uidvalidity AND c2.uid = d.uid
              )
             WHERE (?1 IS NULL OR d.account_id = ?1)
               AND (?2 IS NULL OR COALESCE(c.route, d.route) = ?2)
               AND (?3 IS NULL OR (CASE WHEN c.revision IS NOT NULL THEN 'decided' ELSE d.status END) = ?3)
             ORDER BY d.updated_at DESC, d.account_id ASC, d.folder ASC, d.uid DESC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![account_id, route, status, limit as i64], |row| {
            Ok(MailEngineDecisionSummary {
                account_id: row.get(0)?,
                folder: row.get(1)?,
                uidvalidity: row.get::<_, i64>(2)? as u32,
                uid: row.get::<_, i64>(3)? as u32,
                model_status: row.get(4)?,
                status: row.get(5)?,
                route: row.get(6)?,
                effective_route: row.get(7)?,
                route_probability: row.get(8)?,
                route_confidence: row.get(9)?,
                urgency: row.get(10)?,
                effective_urgency: row.get(11)?,
                notify_user_probability: row.get(12)?,
                requires_reply_probability: row.get(13)?,
                bulk_or_subscription_probability: row.get(14)?,
                execution_status: row.get(15)?,
                executed_action: row.get(16)?,
                model_error_code: row.get(17)?,
                error_code: row.get(18)?,
                correction_revision: row.get::<_, i64>(19)? as u64,
                decided_at: row.get(20)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn list_mail_engine_decision_display(
        &self,
        account_id: Option<&str>,
        route: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MailEngineDecisionDisplay>> {
        let decisions = self.list_mail_engine_decisions(account_id, route, status, limit)?;
        decisions
            .into_iter()
            .map(|decision| {
                let metadata: Option<(String, String, Option<String>)> = self
                    .conn()
                    .query_row(
                        "SELECT from_addr, subject, date
                         FROM indexed_message_summaries
                         WHERE account_id = ?1 AND folder = ?2
                           AND uidvalidity = ?3 AND uid = ?4",
                        params![
                            decision.account_id,
                            decision.folder,
                            i64::from(decision.uidvalidity),
                            i64::from(decision.uid),
                        ],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                let (metadata_state, from_addr, subject, date) = match metadata {
                    Some((from_addr, subject, date)) => (
                        "available".to_string(),
                        Some(from_addr),
                        Some(subject),
                        date,
                    ),
                    None => ("unavailable".to_string(), None, None, None),
                };
                Ok(MailEngineDecisionDisplay {
                    decision,
                    metadata_state,
                    from_addr,
                    subject,
                    date,
                })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn correct_current_mail_engine_decision(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        expected_revision: u64,
        route: &str,
        urgency: &str,
        actor: &str,
    ) -> Result<Option<u64>> {
        validate_mail_engine_route(route)?;
        validate_mail_engine_urgency(urgency)?;
        validate_token(actor, "actor")?;
        let tx = self.conn().unchecked_transaction()?;
        let row: Option<(i64, String, String, i64)> = tx
            .query_row(
                "SELECT d.uidvalidity, d.status, d.execution_status,
                        COALESCE(MAX(c.revision), 0)
                 FROM mail_engine_decisions d
                 JOIN mail_engine_mailboxes m
                   ON m.account_id = d.account_id AND m.folder = d.folder
                  AND m.uidvalidity = d.uidvalidity
                 LEFT JOIN mail_engine_decision_corrections c
                   ON c.account_id = d.account_id AND c.folder = d.folder
                  AND c.uidvalidity = d.uidvalidity AND c.uid = d.uid
                 WHERE d.account_id = ?1 AND d.folder = ?2 AND d.uid = ?3
                 GROUP BY d.uidvalidity, d.status, d.execution_status",
                params![account_id, folder, i64::from(uid)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((uidvalidity, status, execution_status, current_revision)) = row else {
            tx.commit()?;
            return Ok(None);
        };
        if current_revision as u64 != expected_revision
            || !matches!(status.as_str(), "decided" | "review")
            || execution_status == "executing"
        {
            tx.commit()?;
            return Ok(None);
        }
        let next_revision = current_revision + 1;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO mail_engine_decision_corrections
             (account_id, folder, uidvalidity, uid, revision, route, urgency, actor)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                account_id,
                folder,
                uidvalidity,
                i64::from(uid),
                next_revision,
                route,
                urgency,
                actor,
            ],
        )?;
        if inserted != 1 {
            tx.commit()?;
            return Ok(None);
        }
        if route != "junk" {
            tx.execute(
                "UPDATE mail_engine_decisions
                 SET execution_status = 'cancelled',
                     executed_action = 'cancelled_by_human_correction',
                     updated_at = datetime('now')
                 WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4
                   AND route = 'junk' AND execution_status = 'pending'",
                params![account_id, folder, uidvalidity, i64::from(uid)],
            )?;
        }
        tx.commit()?;
        Ok(Some(next_revision as u64))
    }

    pub fn mail_engine_decision_exists(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u32,
        uid: u32,
    ) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM mail_engine_decisions
                 WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4",
                params![account_id, folder, i64::from(uidvalidity), i64::from(uid)],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn mail_engine_processing_claim_matches(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u32,
        uid: u32,
        input_hash: &str,
    ) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM mail_engine_decisions
                 WHERE account_id = ?1 AND folder = ?2 AND uidvalidity = ?3 AND uid = ?4
                   AND input_hash = ?5 AND status = 'processing'",
                params![
                    account_id,
                    folder,
                    i64::from(uidvalidity),
                    i64::from(uid),
                    input_hash,
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn upsert_mail_engine_sender_stats(
        &self,
        account_id: &str,
        sender_address: &str,
        sender_domain: &str,
        stats: &MailEngineSenderStats,
    ) -> Result<()> {
        let sender_hash = mail_engine_hash(&sender_address.trim().to_ascii_lowercase());
        let domain_hash = mail_engine_hash(&sender_domain.trim().to_ascii_lowercase());
        self.conn().execute(
            "INSERT INTO mail_engine_sender_stats (
                account_id, sender_hash, domain_hash, total_received, read_count,
                unread_count, junk_count, replied_thread_count, outbound_count,
                inbound_count, distinct_thread_count, first_seen, last_seen,
                history_complete, source_version
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(account_id, sender_hash) DO UPDATE SET
                domain_hash = excluded.domain_hash,
                total_received = excluded.total_received,
                read_count = excluded.read_count,
                unread_count = excluded.unread_count,
                junk_count = excluded.junk_count,
                replied_thread_count = excluded.replied_thread_count,
                outbound_count = excluded.outbound_count,
                inbound_count = excluded.inbound_count,
                distinct_thread_count = excluded.distinct_thread_count,
                first_seen = excluded.first_seen,
                last_seen = excluded.last_seen,
                history_complete = excluded.history_complete,
                source_version = excluded.source_version,
                updated_at = datetime('now')",
            params![
                account_id,
                sender_hash,
                domain_hash,
                stats.total_received as i64,
                stats.read_count as i64,
                stats.unread_count as i64,
                stats.junk_count as i64,
                stats.replied_thread_count as i64,
                stats.outbound_count as i64,
                stats.inbound_count as i64,
                stats.distinct_thread_count as i64,
                stats.first_seen,
                stats.last_seen,
                i64::from(stats.history_complete),
                i64::from(stats.source_version),
            ],
        )?;
        Ok(())
    }

    pub fn get_mail_engine_sender_stats(
        &self,
        account_id: &str,
        sender_address: &str,
    ) -> Result<Option<MailEngineSenderStats>> {
        let sender_hash = mail_engine_hash(&sender_address.trim().to_ascii_lowercase());
        self.conn()
            .query_row(
                "SELECT total_received, read_count, unread_count, junk_count,
                        replied_thread_count, outbound_count, inbound_count,
                        distinct_thread_count, first_seen, last_seen,
                        history_complete, source_version
                 FROM mail_engine_sender_stats
                 WHERE account_id = ?1 AND sender_hash = ?2",
                params![account_id, sender_hash],
                |row| {
                    Ok(MailEngineSenderStats {
                        total_received: row.get::<_, i64>(0)? as u64,
                        read_count: row.get::<_, i64>(1)? as u64,
                        unread_count: row.get::<_, i64>(2)? as u64,
                        junk_count: row.get::<_, i64>(3)? as u64,
                        replied_thread_count: row.get::<_, i64>(4)? as u64,
                        outbound_count: row.get::<_, i64>(5)? as u64,
                        inbound_count: row.get::<_, i64>(6)? as u64,
                        distinct_thread_count: row.get::<_, i64>(7)? as u64,
                        first_seen: row.get(8)?,
                        last_seen: row.get(9)?,
                        history_complete: row.get::<_, i64>(10)? != 0,
                        source_version: row.get::<_, i64>(11)? as u32,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Derive the sender-history snapshot supplied to every Jev call from the
    /// local thread cache plus indexed mailbox flags. Both scans are bounded;
    /// hitting either cap makes `history_complete` false rather than asserting
    /// that missing history does not exist.
    pub fn derive_mail_engine_sender_stats(
        &self,
        account_id: &str,
        sender_address: &str,
    ) -> Result<MailEngineSenderStats> {
        const ROW_LIMIT: usize = 5_000;
        let target = normalize_single_address(sender_address).ok_or_else(|| {
            StoreError::Config("mail engine sender address is not parseable".into())
        })?;
        let mut stats = MailEngineSenderStats {
            source_version: MAIL_ENGINE_SCHEMA_VERSION,
            ..Default::default()
        };
        let mut thread_directions: HashMap<String, (bool, bool)> = HashMap::new();
        let mut thread_rows = 0usize;
        let mut thread_truncated = false;
        {
            let mut stmt = self.conn().prepare(
                "SELECT tm.thread_id, tm.from_address, tm.to_addresses,
                        tm.cc_addresses, tm.bcc_addresses, tm.date,
                        tm.is_outbound
                 FROM thread_messages tm
                 JOIN threads t ON t.thread_id = tm.thread_id
                 WHERE t.account_id = ?1
                 ORDER BY tm.id DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![account_id, (ROW_LIMIT + 1) as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)? != 0,
                ))
            })?;
            for row in rows {
                if thread_rows == ROW_LIMIT {
                    thread_truncated = true;
                    break;
                }
                thread_rows += 1;
                let (thread_id, from, to, cc, bcc, date, outbound) = row?;
                let from_matches = from
                    .as_deref()
                    .and_then(normalize_single_address)
                    .is_some_and(|address| address == target);
                let recipient_matches = [to.as_deref(), cc.as_deref(), bcc.as_deref()]
                    .into_iter()
                    .flatten()
                    .flat_map(crate::address_book::parse_address_list)
                    .any(|address| address.email == target);
                if !outbound && from_matches {
                    stats.inbound_count += 1;
                    observe_timestamp(&mut stats, date.as_deref());
                    thread_directions.entry(thread_id).or_default().0 = true;
                } else if outbound && recipient_matches {
                    stats.outbound_count += 1;
                    observe_timestamp(&mut stats, date.as_deref());
                    thread_directions.entry(thread_id).or_default().1 = true;
                }
            }
        }
        stats.total_received = stats.inbound_count;
        stats.distinct_thread_count = thread_directions.len() as u64;
        stats.replied_thread_count = thread_directions
            .values()
            .filter(|(inbound, outbound)| *inbound && *outbound)
            .count() as u64;

        let mut index_rows = 0usize;
        let mut index_truncated = false;
        let mut indexed_message_keys = HashSet::new();
        {
            let mut stmt = self.conn().prepare(
                "SELECT folder, uidvalidity, uid, from_addr, flags_json
                 FROM indexed_message_summaries
                 WHERE account_id = ?1
                 ORDER BY indexed_at DESC, uid DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![account_id, (ROW_LIMIT + 1) as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            for row in rows {
                if index_rows == ROW_LIMIT {
                    index_truncated = true;
                    break;
                }
                index_rows += 1;
                let (folder, uidvalidity, uid, from, flags_json) = row?;
                if normalize_single_address(&from).as_deref() != Some(target.as_str()) {
                    continue;
                }
                if !indexed_message_keys.insert((folder.clone(), uidvalidity, uid)) {
                    continue;
                }
                let flags: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();
                let seen = flags.iter().any(|flag| {
                    flag.eq_ignore_ascii_case("\\Seen") || flag.eq_ignore_ascii_case("Seen")
                });
                let junk = folder.to_ascii_lowercase().contains("junk")
                    || folder.to_ascii_lowercase().contains("spam")
                    || flags.iter().any(|flag| {
                        matches!(
                            flag.to_ascii_lowercase().as_str(),
                            "junk" | "\\junk" | "$junk" | "custom(\"junk\")" | "custom(\"$junk\")"
                        )
                    });
                if seen {
                    stats.read_count += 1;
                } else {
                    stats.unread_count += 1;
                }
                if junk {
                    stats.junk_count += 1;
                }
            }
        }
        stats.history_complete = !thread_truncated
            && !index_truncated
            && stats.read_count + stats.unread_count >= stats.total_received;
        Ok(stats)
    }

    /// Aggregate only counts and stable state. No sender, subject, body,
    /// Message-ID, model input, or model answer JSON is returned.
    pub fn list_mail_engine_status(
        &self,
        account_id: Option<&str>,
    ) -> Result<Vec<MailEngineStatus>> {
        let mut stmt = self.conn().prepare(
            "SELECT m.account_id, m.folder, m.uidvalidity, m.last_seen_uid,
                    m.baseline_reason,
                    COUNT(d.uid) AS decision_count,
                    SUM(CASE WHEN COALESCE(c.route, d.route) = 'review' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN d.status = 'processing' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN COALESCE(c.route, d.route) = 'junk' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN COALESCE(c.route, d.route) = 'follow_up' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN COALESCE(c.route, d.route) = 'important' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN COALESCE(c.route, d.route) = 'routine' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN COALESCE(c.route, d.route) = 'digest_news' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN COALESCE(c.route, d.route) = 'unsubscribe_candidate' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN COALESCE(d.notify_user_probability, 0) >= 0.9
                             AND COALESCE(c.urgency, d.urgency) IN ('urgent', 'critical') THEN 1 ELSE 0 END),
                    m.last_error
             FROM mail_engine_mailboxes m
             LEFT JOIN mail_engine_decisions d
               ON d.account_id = m.account_id AND d.folder = m.folder
              AND d.uidvalidity = m.uidvalidity
             LEFT JOIN mail_engine_decision_corrections c
               ON c.account_id = d.account_id AND c.folder = d.folder
              AND c.uidvalidity = d.uidvalidity AND c.uid = d.uid
              AND c.revision = (
                  SELECT MAX(c2.revision) FROM mail_engine_decision_corrections c2
                  WHERE c2.account_id = d.account_id AND c2.folder = d.folder
                    AND c2.uidvalidity = d.uidvalidity AND c2.uid = d.uid
              )
             WHERE (?1 IS NULL OR m.account_id = ?1)
             GROUP BY m.account_id, m.folder
             ORDER BY m.account_id, m.folder",
        )?;
        let rows = stmt.query_map([account_id], |row| {
            Ok(MailEngineStatus {
                account_id: row.get(0)?,
                folder: row.get(1)?,
                uidvalidity: row.get::<_, i64>(2)? as u32,
                last_seen_uid: row.get::<_, i64>(3)? as u32,
                baseline_reason: row.get(4)?,
                decision_count: row.get::<_, i64>(5)? as u64,
                review_count: row.get::<_, i64>(6)? as u64,
                processing_count: row.get::<_, i64>(7)? as u64,
                junk_count: row.get::<_, i64>(8)? as u64,
                follow_up_count: row.get::<_, i64>(9)? as u64,
                important_count: row.get::<_, i64>(10)? as u64,
                routine_count: row.get::<_, i64>(11)? as u64,
                digest_news_count: row.get::<_, i64>(12)? as u64,
                unsubscribe_candidate_count: row.get::<_, i64>(13)? as u64,
                notification_count: row.get::<_, i64>(14)? as u64,
                last_error: row.get(15)?,
            })
        })?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }
}

pub fn mail_engine_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    format!("{digest:x}")
}

fn normalize_single_address(value: &str) -> Option<String> {
    crate::address_book::parse_address_list(value)
        .into_iter()
        .next()
        .map(|address| address.email)
}

fn observe_timestamp(stats: &mut MailEngineSenderStats, timestamp: Option<&str>) {
    let Some(timestamp) = timestamp.filter(|value| !value.trim().is_empty()) else {
        return;
    };
    if stats
        .first_seen
        .as_deref()
        .is_none_or(|first| timestamp < first)
    {
        stats.first_seen = Some(timestamp.to_string());
    }
    if stats
        .last_seen
        .as_deref()
        .is_none_or(|last| timestamp > last)
    {
        stats.last_seen = Some(timestamp.to_string());
    }
}

fn validate_decision_for_storage(decision: &NewMailEngineDecision<'_>) -> Result<()> {
    for (value, field) in [
        (decision.status, "status"),
        (decision.route, "route"),
        (decision.urgency, "urgency"),
    ] {
        validate_token(value, field)?;
    }
    if decision.decision_json.len() > MAX_DECISION_JSON_BYTES {
        return Err(StoreError::Config(format!(
            "mail engine decision_json exceeds {MAX_DECISION_JSON_BYTES} bytes"
        )));
    }
    let parsed: serde_json::Value = serde_json::from_str(decision.decision_json)?;
    if !parsed.is_object() {
        return Err(StoreError::Config(
            "mail engine decision_json must be an object".into(),
        ));
    }
    for (value, field) in [
        (decision.route_probability, "route_probability"),
        (decision.route_confidence, "route_confidence"),
        (decision.notify_user_probability, "notify_user_probability"),
        (
            decision.requires_reply_probability,
            "requires_reply_probability",
        ),
        (
            decision.bulk_or_subscription_probability,
            "bulk_or_subscription_probability",
        ),
    ] {
        validate_optional_probability(value, field)?;
    }
    Ok(())
}

fn validate_token(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(StoreError::Config(format!(
            "mail engine {field} must be a lowercase token"
        )));
    }
    Ok(())
}

fn validate_mail_engine_route(value: &str) -> Result<()> {
    if matches!(
        value,
        "junk"
            | "follow_up"
            | "important"
            | "routine"
            | "digest_news"
            | "unsubscribe_candidate"
            | "review"
    ) {
        Ok(())
    } else {
        Err(StoreError::Config("invalid mail engine route".into()))
    }
}

fn validate_mail_engine_urgency(value: &str) -> Result<()> {
    if matches!(value, "not_urgent" | "urgent" | "critical") {
        Ok(())
    } else {
        Err(StoreError::Config("invalid mail engine urgency".into()))
    }
}

fn validate_optional_probability(value: Option<f64>, field: &str) -> Result<()> {
    if let Some(value) = value
        && (!value.is_finite() || !(0.0..=1.0).contains(&value))
    {
        return Err(StoreError::Config(format!(
            "mail engine {field} must be finite and between 0 and 1"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_memory().unwrap()
    }

    #[test]
    fn first_observation_baselines_and_second_observation_is_new_only() {
        let db = db();
        assert_eq!(
            db.plan_mail_engine_scan("acct", "INBOX", 10, 100).unwrap(),
            MailboxScanPlan::Baseline {
                uidvalidity: 10,
                last_seen_uid: 100
            }
        );
        assert_eq!(
            db.plan_mail_engine_scan("acct", "INBOX", 10, 103).unwrap(),
            MailboxScanPlan::NewRange {
                uidvalidity: 10,
                after_uid: 100,
                through_uid: 103
            }
        );
        assert!(
            db.advance_mail_engine_watermark("acct", "INBOX", 10, 103)
                .unwrap()
        );
        assert_eq!(
            db.plan_mail_engine_scan("acct", "INBOX", 10, 103).unwrap(),
            MailboxScanPlan::Current
        );
    }

    #[test]
    fn uidvalidity_change_rebaselines_without_replay() {
        let db = db();
        db.plan_mail_engine_scan("acct", "INBOX", 10, 100).unwrap();
        let plan = db.plan_mail_engine_scan("acct", "INBOX", 11, 7).unwrap();
        assert_eq!(
            plan,
            MailboxScanPlan::Rebaseline {
                uidvalidity: 11,
                last_seen_uid: 7,
                reason: "uidvalidity_changed".into()
            }
        );
        assert!(
            !db.advance_mail_engine_watermark("acct", "INBOX", 10, 101)
                .unwrap()
        );
        assert_eq!(
            db.plan_mail_engine_scan("acct", "INBOX", 11, 7).unwrap(),
            MailboxScanPlan::Current
        );
    }

    fn decision<'a>() -> NewMailEngineDecision<'a> {
        NewMailEngineDecision {
            account_id: "acct",
            folder: "INBOX",
            uidvalidity: 10,
            uid: 101,
            input_hash: "0123456789abcdef",
            model: "typesafe/jev-1.13",
            status: "decided",
            route: "follow_up",
            route_probability: Some(0.9),
            route_confidence: Some(0.9),
            urgency: "not_urgent",
            notify_user_probability: Some(0.1),
            requires_reply_probability: Some(0.95),
            bulk_or_subscription_probability: Some(0.1),
            decision_json: r#"{"route":{"choice":"follow_up"}}"#,
        }
    }

    #[test]
    fn decision_insert_is_idempotent() {
        let db = db();
        assert!(
            db.insert_mail_engine_decision_if_absent(&decision())
                .unwrap()
        );
        assert!(
            !db.insert_mail_engine_decision_if_absent(&decision())
                .unwrap()
        );
        assert!(
            db.mail_engine_decision_exists("acct", "INBOX", 10, 101)
                .unwrap()
        );
    }

    #[test]
    fn pre_request_claim_prevents_duplicate_spend_and_finalizes_once() {
        let db = db();
        let claim = MailEngineDecisionClaim {
            account_id: "acct",
            folder: "INBOX",
            uidvalidity: 10,
            uid: 101,
            input_hash: "0123456789abcdef",
            model: "typesafe/jev-1.13",
        };
        assert!(db.claim_mail_engine_decision(&claim).unwrap());
        assert!(!db.claim_mail_engine_decision(&claim).unwrap());
        assert!(db.finalize_mail_engine_decision(&decision()).unwrap());
        assert!(!db.finalize_mail_engine_decision(&decision()).unwrap());
        assert_eq!(
            db.get_mail_engine_decision_execution("acct", "INBOX", 10, 101)
                .unwrap(),
            Some(MailEngineDecisionRecovery {
                status: "decided".into(),
                route: "follow_up".into(),
                execution_status: "not_requested".into(),
                urgency: "not_urgent".into(),
                notify_user_probability: Some(0.1),
            })
        );
    }

    #[test]
    fn operator_can_release_or_explicitly_retry_current_held_decisions() {
        let db = db();
        db.plan_mail_engine_scan("acct", "INBOX", 10, 110).unwrap();
        let claim = MailEngineDecisionClaim {
            account_id: "acct",
            folder: "INBOX",
            uidvalidity: 10,
            uid: 111,
            input_hash: "held-input",
            model: "typesafe/jev-1.13",
        };
        assert!(db.claim_mail_engine_decision(&claim).unwrap());
        assert!(
            db.resolve_current_mail_engine_processing_as_review("acct", "INBOX", 111)
                .unwrap()
        );
        let released = db
            .list_mail_engine_decisions(Some("acct"), Some("review"), Some("review"), 10)
            .unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(
            released[0].error_code.as_deref(),
            Some("processing_recovered_for_review")
        );
        assert!(
            !db.resolve_current_mail_engine_processing_as_review("acct", "INBOX", 111)
                .unwrap()
        );
        assert_eq!(
            db.prepare_current_mail_engine_safe_retry("acct", "INBOX", 111)
                .unwrap(),
            None
        );

        let mut safe_review = decision();
        safe_review.uid = 112;
        safe_review.status = "review";
        safe_review.route = "review";
        safe_review.route_probability = None;
        safe_review.route_confidence = None;
        safe_review.notify_user_probability = None;
        safe_review.requires_reply_probability = None;
        safe_review.bulk_or_subscription_probability = None;
        safe_review.decision_json = r#"{"error_code":"openrouter_api_key_missing"}"#;
        assert!(
            db.insert_mail_engine_decision_if_absent(&safe_review)
                .unwrap()
        );
        db.advance_mail_engine_watermark("acct", "INBOX", 10, 112)
            .unwrap();
        assert_eq!(
            db.prepare_current_mail_engine_safe_retry("acct", "INBOX", 112)
                .unwrap(),
            Some(10)
        );
        assert!(
            db.mail_engine_decision_exists("acct", "INBOX", 10, 112)
                .unwrap()
        );
        assert_eq!(
            db.get_mail_engine_decision_execution("acct", "INBOX", 10, 112)
                .unwrap()
                .unwrap()
                .status,
            "processing"
        );
        assert_eq!(
            db.plan_mail_engine_scan("acct", "INBOX", 10, 112).unwrap(),
            MailboxScanPlan::Current
        );

        safe_review.uid = 113;
        assert!(
            db.insert_mail_engine_decision_if_absent(&safe_review)
                .unwrap()
        );
        assert_eq!(
            db.correct_current_mail_engine_decision(
                "acct",
                "INBOX",
                113,
                0,
                "important",
                "urgent",
                "dashboard",
            )
            .unwrap(),
            Some(1)
        );
        assert_eq!(
            db.prepare_current_mail_engine_safe_retry("acct", "INBOX", 113)
                .unwrap(),
            None,
            "a human-corrected review must never be deleted for model retry"
        );
        let corrected_review = db
            .list_mail_engine_decisions(Some("acct"), Some("important"), None, 20)
            .unwrap()
            .into_iter()
            .find(|item| item.uid == 113)
            .unwrap();
        assert_eq!(corrected_review.model_status, "review");
        assert_eq!(corrected_review.status, "decided");
        assert_eq!(
            corrected_review.model_error_code.as_deref(),
            Some("openrouter_api_key_missing")
        );
        assert_eq!(corrected_review.error_code, None);
        assert!(
            db.mail_engine_decision_exists("acct", "INBOX", 10, 113)
                .unwrap()
        );
    }

    #[test]
    fn stale_processing_claims_park_once_without_another_request() {
        let db = db();
        db.plan_mail_engine_scan("acct", "INBOX", 10, 100).unwrap();
        assert!(
            db.claim_mail_engine_decision(&MailEngineDecisionClaim {
                account_id: "acct",
                folder: "INBOX",
                uidvalidity: 10,
                uid: 101,
                input_hash: "interrupted",
                model: "typesafe/jev-1.13",
            })
            .unwrap()
        );
        assert_eq!(db.recover_stale_mail_engine_processing(600).unwrap(), 0);
        db.conn()
            .execute(
                "UPDATE mail_engine_decisions SET updated_at = datetime('now', '-11 minutes')",
                [],
            )
            .unwrap();
        assert_eq!(db.recover_stale_mail_engine_processing(600).unwrap(), 1);
        assert_eq!(db.recover_stale_mail_engine_processing(600).unwrap(), 0);
        let review = db
            .list_mail_engine_decisions(Some("acct"), Some("review"), Some("review"), 10)
            .unwrap();
        assert_eq!(review.len(), 1);
        assert_eq!(
            review[0].error_code.as_deref(),
            Some("decision_interrupted")
        );
        assert!(db.recover_stale_mail_engine_processing(59).is_err());
    }

    #[test]
    fn pending_junk_is_available_for_later_apply_pass() {
        let db = db();
        for uid in [3_u32, 1, 2] {
            let mut item = decision();
            item.uid = uid;
            item.route = "junk";
            assert!(db.insert_mail_engine_decision_if_absent(&item).unwrap());
        }
        assert_eq!(
            db.list_pending_mail_engine_junk("acct", "INBOX", 10, 2)
                .unwrap(),
            vec![1, 2]
        );
        assert!(
            db.claim_mail_engine_execution("acct", "INBOX", 10, 1)
                .unwrap()
        );
        assert!(
            !db.claim_mail_engine_execution("acct", "INBOX", 10, 1)
                .unwrap()
        );
        db.set_mail_engine_execution(
            "acct",
            "INBOX",
            10,
            1,
            "completed",
            Some("move_to_spam"),
            None,
        )
        .unwrap();
        assert_eq!(
            db.list_pending_mail_engine_junk("acct", "INBOX", 10, 10)
                .unwrap(),
            vec![2, 3]
        );
    }

    #[test]
    fn digest_queue_is_bounded_filtered_and_contains_only_message_handles() {
        let db = db();
        db.plan_mail_engine_scan("acct", "INBOX", 10, 100).unwrap();
        db.plan_mail_engine_scan("other", "INBOX", 10, 100).unwrap();
        for (uid, route, account_id, uidvalidity) in [
            (5_u32, "digest_news", "acct", 9_u32),
            (4_u32, "digest_news", "acct", 10),
            (2_u32, "important", "acct", 10),
            (3_u32, "digest_news", "other", 10),
            (1_u32, "digest_news", "acct", 10),
        ] {
            let mut item = decision();
            item.uid = uid;
            item.route = route;
            item.account_id = account_id;
            item.uidvalidity = uidvalidity;
            assert!(db.insert_mail_engine_decision_if_absent(&item).unwrap());
        }

        let queue = db.list_mail_engine_digest_queue(Some("acct"), 1).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].account_id, "acct");
        assert_eq!(queue[0].folder, "INBOX");
        assert_eq!(queue[0].uidvalidity, 10);
        assert_eq!(queue[0].uid, 4);
        assert_eq!(queue[0].urgency, "not_urgent");
        let rendered = serde_json::to_string(&queue).unwrap();
        for forbidden in ["decision_json", "input_hash", "model", "subject", "sender"] {
            assert!(!rendered.contains(forbidden));
        }
        assert_eq!(
            db.list_mail_engine_digest_queue(None, 100).unwrap().len(),
            3
        );
        assert_eq!(
            db.count_pending_mail_engine_digest(Some("acct")).unwrap(),
            2
        );
        let consumed = db
            .consume_mail_engine_digest(&[
                MailEngineDigestKey {
                    account_id: "acct".into(),
                    folder: "INBOX".into(),
                    uidvalidity: 10,
                    uid: 4,
                },
                MailEngineDigestKey {
                    account_id: "acct".into(),
                    folder: "INBOX".into(),
                    uidvalidity: 10,
                    uid: 2,
                },
            ])
            .unwrap();
        assert_eq!(consumed, 1);
        assert_eq!(
            db.count_pending_mail_engine_digest(Some("acct")).unwrap(),
            1
        );
        assert_eq!(
            db.list_mail_engine_digest_queue(Some("acct"), 100).unwrap()[0].uid,
            1
        );
        assert_eq!(
            db.consume_mail_engine_digest(&[MailEngineDigestKey {
                account_id: "acct".into(),
                folder: "INBOX".into(),
                uidvalidity: 10,
                uid: 4,
            }])
            .unwrap(),
            0
        );
        assert!(db.list_mail_engine_digest_queue(None, 0).is_err());
        assert!(db.list_mail_engine_digest_queue(None, 101).is_err());
    }

    #[test]
    fn decision_list_is_current_filterable_and_privacy_minimized() {
        let db = db();
        db.plan_mail_engine_scan("acct", "INBOX", 10, 100).unwrap();
        for (uid, route, uidvalidity) in [
            (1_u32, "follow_up", 10_u32),
            (2, "important", 10),
            (3, "junk", 10),
            (4, "important", 9),
        ] {
            let mut item = decision();
            item.uid = uid;
            item.route = route;
            item.uidvalidity = uidvalidity;
            assert!(db.insert_mail_engine_decision_if_absent(&item).unwrap());
        }

        let decisions = db
            .list_mail_engine_decisions(Some("acct"), Some("important"), Some("decided"), 20)
            .unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].uid, 2);
        assert_eq!(decisions[0].route, "important");
        let rendered = serde_json::to_string(&decisions).unwrap();
        for forbidden in [
            "decision_json",
            "input_hash",
            "typesafe/jev-1.13",
            "subject",
            "sender",
            "private.sender",
        ] {
            assert!(!rendered.contains(forbidden));
        }
        db.conn()
            .execute(
                "INSERT INTO indexed_message_summaries
                 (account_id, folder, uidvalidity, uid, from_addr, subject,
                  flags_json, size, indexed_at)
                 VALUES ('acct', 'INBOX', 10, 2, 'sender@example.test',
                         'Human-visible subject', '[]', 0, datetime('now'))",
                [],
            )
            .unwrap();
        let display = db
            .list_mail_engine_decision_display(Some("acct"), Some("important"), Some("decided"), 20)
            .unwrap();
        assert_eq!(display[0].metadata_state, "available");
        assert_eq!(display[0].subject.as_deref(), Some("Human-visible subject"));
        assert_eq!(
            db.correct_current_mail_engine_decision(
                "acct",
                "INBOX",
                1,
                0,
                "important",
                "critical",
                "cli",
            )
            .unwrap(),
            Some(1)
        );
        assert_eq!(
            db.correct_current_mail_engine_decision(
                "acct",
                "INBOX",
                1,
                0,
                "routine",
                "not_urgent",
                "cli",
            )
            .unwrap(),
            None,
            "a stale revision must not overwrite a human correction"
        );
        let corrected = db
            .list_mail_engine_decisions(Some("acct"), Some("important"), None, 20)
            .unwrap()
            .into_iter()
            .find(|item| item.uid == 1)
            .unwrap();
        assert_eq!(corrected.route, "follow_up");
        assert_eq!(corrected.effective_route, "important");
        assert_eq!(corrected.urgency, "not_urgent");
        assert_eq!(corrected.effective_urgency, "critical");
        assert_eq!(corrected.correction_revision, 1);
        assert_eq!(
            db.correct_current_mail_engine_decision(
                "acct",
                "INBOX",
                3,
                0,
                "routine",
                "not_urgent",
                "dashboard",
            )
            .unwrap(),
            Some(1)
        );
        assert_eq!(
            db.get_mail_engine_decision_execution("acct", "INBOX", 10, 3)
                .unwrap()
                .unwrap()
                .execution_status,
            "cancelled"
        );
        db.set_mail_engine_execution("acct", "INBOX", 10, 3, "pending", None, None)
            .unwrap();
        let recovery = db
            .get_mail_engine_decision_execution("acct", "INBOX", 10, 3)
            .unwrap()
            .unwrap();
        assert_eq!(recovery.status, "decided");
        assert_eq!(recovery.route, "routine");
        assert_eq!(recovery.urgency, "not_urgent");
        assert_eq!(recovery.execution_status, "pending");
        assert!(
            !db.claim_mail_engine_execution("acct", "INBOX", 10, 3)
                .unwrap(),
            "the execution claim itself must reject a corrected-away-from-junk decision"
        );
        assert!(
            !db.list_pending_mail_engine_junk("acct", "INBOX", 10, 20)
                .unwrap()
                .contains(&3),
            "the executor must use the effective corrected route even if legacy state says pending"
        );
        assert!(db.list_mail_engine_decisions(None, None, None, 0).is_err());
        assert!(
            db.list_mail_engine_decisions(None, None, None, 201)
                .is_err()
        );
    }

    #[test]
    fn sender_stats_are_hashed_and_round_trip() {
        let db = db();
        let stats = MailEngineSenderStats {
            total_received: 9,
            read_count: 6,
            unread_count: 3,
            junk_count: 1,
            replied_thread_count: 2,
            outbound_count: 3,
            inbound_count: 9,
            distinct_thread_count: 4,
            first_seen: Some("2026-01-01T00:00:00Z".into()),
            last_seen: Some("2026-09-19T00:00:00Z".into()),
            history_complete: true,
            source_version: MAIL_ENGINE_SCHEMA_VERSION,
        };
        db.upsert_mail_engine_sender_stats(
            "acct",
            "Private.Sender@Example.Test",
            "Example.Test",
            &stats,
        )
        .unwrap();
        assert_eq!(
            db.get_mail_engine_sender_stats("acct", "private.sender@example.test")
                .unwrap(),
            Some(stats)
        );
        let stored: (String, String) = db
            .conn()
            .query_row(
                "SELECT sender_hash, domain_hash FROM mail_engine_sender_stats",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(!stored.0.contains("private.sender"));
        assert!(!stored.1.contains("example.test"));
    }

    #[test]
    fn status_is_aggregate_and_contains_no_message_content() {
        let db = db();
        db.plan_mail_engine_scan("acct", "INBOX", 10, 100).unwrap();
        for (uid, route) in [(101_u32, "follow_up"), (102, "junk"), (103, "routine")] {
            let mut item = decision();
            item.uid = uid;
            item.route = route;
            db.insert_mail_engine_decision_if_absent(&item).unwrap();
        }
        assert!(
            db.claim_mail_engine_decision(&MailEngineDecisionClaim {
                account_id: "acct",
                folder: "INBOX",
                uidvalidity: 10,
                uid: 104,
                input_hash: "processing-hash",
                model: "typesafe/jev-1.13",
            })
            .unwrap()
        );
        let status = db.list_mail_engine_status(Some("acct")).unwrap();
        let rendered = serde_json::to_string(&status).unwrap();
        assert_eq!(status[0].decision_count, 4);
        assert_eq!(status[0].processing_count, 1);
        assert_eq!(status[0].junk_count, 1);
        assert_eq!(status[0].follow_up_count, 1);
        assert_eq!(status[0].routine_count, 1);
        for forbidden in [
            "private.sender",
            "subject-sentinel",
            "body-sentinel",
            "message-id-sentinel",
            "0123456789abcdef",
            "typesafe/jev-1.13",
        ] {
            assert!(!rendered.contains(forbidden));
        }
    }

    #[test]
    fn sender_history_derives_interactions_replies_and_indexed_flags() {
        let db = db();
        let thread = db
            .create_thread(
                "fixture",
                "2026-01-01T00:00:00Z",
                "2026-01-02T00:00:00Z",
                "acct",
            )
            .unwrap();
        db.upsert_thread_message(
            &thread.thread_id,
            1,
            Some("inbound@fixture.test"),
            None,
            None,
            "INBOX",
            "Sender <sender@example.test>",
            "me@example.test",
            None,
            None,
            "2026-01-01T00:00:00Z",
            "fixture",
            false,
            None,
        )
        .unwrap();
        db.upsert_thread_message(
            &thread.thread_id,
            2,
            Some("outbound@fixture.test"),
            Some("inbound@fixture.test"),
            None,
            "Sent",
            "me@example.test",
            "sender@example.test",
            None,
            None,
            "2026-01-02T00:00:00Z",
            "fixture",
            true,
            None,
        )
        .unwrap();
        db.upsert_indexed_message_summaries(
            "acct",
            "INBOX",
            10,
            &[crate::models::IndexedMessageInput {
                uid: 1,
                message_id: None,
                from_addr: "Sender <sender@example.test>".into(),
                to_addr: "me@example.test".into(),
                subject: "fixture".into(),
                date: Some("Thu, 1 Jan 2026 00:00:00 +0000".into()),
                flags: vec!["\\Seen".into(), "Custom(\"$Junk\")".into()],
                size: 100,
                snippet: None,
                thread_id: Some(thread.thread_id),
            }],
        )
        .unwrap();

        let stats = db
            .derive_mail_engine_sender_stats("acct", "sender@example.test")
            .unwrap();
        assert_eq!(stats.total_received, 1);
        assert_eq!(stats.inbound_count, 1);
        assert_eq!(stats.outbound_count, 1);
        assert_eq!(stats.distinct_thread_count, 1);
        assert_eq!(stats.replied_thread_count, 1);
        assert_eq!(stats.read_count, 1);
        assert_eq!(stats.unread_count, 0);
        assert_eq!(stats.junk_count, 1);
        assert!(stats.history_complete);
    }

    #[test]
    fn invalid_probability_and_oversized_json_are_rejected() {
        let db = db();
        let mut invalid = decision();
        invalid.route_probability = Some(1.1);
        assert!(db.insert_mail_engine_decision_if_absent(&invalid).is_err());

        let oversized = format!("{{\"x\":\"{}\"}}", "x".repeat(MAX_DECISION_JSON_BYTES));
        let mut invalid = decision();
        invalid.decision_json = &oversized;
        assert!(db.insert_mail_engine_decision_if_absent(&invalid).is_err());
    }
}
