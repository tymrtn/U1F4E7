// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Threat verdicts in the reader: the banner's data, "Mark safe", "Report",
//! scan-on-open, and the periodic per-account scan.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use envelope_email_store::Database;
use envelope_email_store::models::MessageSummary;
use envelope_email_transport::rule_exec::RunAccount;
use envelope_email_transport::threat::persist::{self, StoredVerdict, VerdictTarget};
use envelope_email_transport::threat::report;
use envelope_email_transport::threat::{self, ThreatConfig, ThreatVerdict};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::handlers::rules::{DashboardDb, DashboardMailbox};
use crate::state::AppState;

/// Newest INBOX messages the periodic scan looks at per account and tick.
const SWEEP_WINDOW: u32 = 50;
/// Most messages one account scans per tick (the rest wait a tick).
const SWEEP_MAX_SCANS: usize = 25;
/// How often the sweep wakes to check each account's timer.
pub const SWEEP_TICK: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
pub struct FolderQuery {
    #[serde(default = "default_folder")]
    pub folder: String,
}

fn default_folder() -> String {
    "INBOX".to_string()
}

fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (status, Json(json!({"code": code, "error": message.into()}))).into_response()
}

/// The reader's view of a verdict.
pub fn verdict_view(
    db: &Database,
    account_id: &str,
    message_id: Option<&str>,
    verdict: &ThreatVerdict,
) -> Value {
    let tags: Vec<String> = message_id
        .and_then(|mid| db.get_tags(account_id, mid).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.tag)
        .filter(|t| t.starts_with("threat:"))
        .collect();
    json!({
        "level": verdict.level,
        "score": verdict.score,
        "signals": verdict.signals,
        "explain": threat::explain(verdict),
        "engine_version": verdict.engine_version,
        "computed_at": verdict.computed_at,
        "marked_safe": tags.iter().any(|t| t == threat::TAG_FALSE_POSITIVE),
        "malware": tags.iter().any(|t| t == threat::TAG_MALWARE),
        "tags": tags,
    })
}

fn stored_view(db: &Database, account_id: &str, stored: &StoredVerdict) -> Value {
    verdict_view(
        db,
        account_id,
        stored.message_id.as_deref(),
        &stored.verdict,
    )
}

/// Scan on open (when `threat.on_read` and no current verdict) and return
/// the banner view. `Ok(None)` when the engine is off or has no verdict.
pub fn verdict_for_open(
    db: &Database,
    account_id: &str,
    account_address: &str,
    folder: &str,
    uid: u32,
    raw: &[u8],
) -> anyhow::Result<Option<Value>> {
    let config = ThreatConfig::load()?;
    let verdict =
        persist::verdict_on_open(db, account_id, account_address, folder, uid, raw, &config)?;
    let message_id =
        persist::stored_verdict_for_uid(db, account_id, folder, uid)?.and_then(|s| s.message_id);
    Ok(verdict.map(|v| verdict_view(db, account_id, message_id.as_deref(), &v)))
}

/// `GET /api/accounts/{id}/messages/{uid}/threat` — the stored verdict only.
pub async fn show(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Query(q): Query<FolderQuery>,
) -> Response {
    let db = state.db.lock().await;
    match persist::stored_verdict_for_uid(&db, &account_id, &q.folder, uid) {
        Ok(Some(stored)) => {
            Json(json!({"threat": stored_view(&db, &account_id, &stored)})).into_response()
        }
        Ok(None) => Json(json!({"threat": null})).into_response(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            format!("{e:#}"),
        ),
    }
}

/// `POST /api/accounts/{id}/messages/{uid}/threat/mark-safe`.
pub async fn mark_safe(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Query(q): Query<FolderQuery>,
) -> Response {
    let db = state.db.lock().await;
    let stored = match persist::stored_verdict_for_uid(&db, &account_id, &q.folder, uid) {
        Ok(Some(stored)) => stored,
        Ok(None) => {
            return error(
                StatusCode::CONFLICT,
                "not_scanned",
                "this message has no threat verdict to override",
            );
        }
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "store_error",
                format!("{e:#}"),
            );
        }
    };
    let Some(message_id) = stored.message_id.as_deref() else {
        return error(
            StatusCode::CONFLICT,
            "no_message_id",
            "the message has no Message-ID to tag",
        );
    };
    let target = VerdictTarget {
        account_id: &account_id,
        folder: &q.folder,
        uid,
        message_id: Some(message_id),
    };
    if let Err(e) = persist::mark_safe(&db, &target, "reader", None) {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            format!("{e:#}"),
        );
    }
    Json(json!({"status": "marked_safe", "threat": stored_view(&db, &account_id, &stored)}))
        .into_response()
}

/// `POST /api/accounts/{id}/messages/{uid}/threat/report` — a draft to
/// `threat.report_to` with the original attached. Never sends.
pub async fn report_draft(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Query(q): Query<FolderQuery>,
) -> Response {
    let config = match ThreatConfig::load() {
        Ok(config) => config,
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "config_invalid",
                format!("{e:#}"),
            );
        }
    };
    let (client_arc, creds) = match state.get_or_create_imap(&account_id).await {
        Ok(c) => c,
        Err(e) => return error(StatusCode::BAD_GATEWAY, "imap_error", format!("{e:#}")),
    };
    let raw = {
        let mut client = client_arc.lock().await;
        envelope_email_transport::imap::fetch_raw_message(&mut client, &q.folder, uid).await
    };
    let raw = match raw {
        Ok(Some(raw)) => raw,
        Ok(None) => return error(StatusCode::NOT_FOUND, "not_found", "message not found"),
        Err(e) => {
            state.evict_imap(&account_id).await;
            return error(StatusCode::BAD_GATEWAY, "imap_error", format!("{e}"));
        }
    };

    let (stored, targets, cached_folder) = {
        let db = state.db.lock().await;
        let stored = persist::stored_verdict_for_uid(&db, &account_id, &q.folder, uid)
            .ok()
            .flatten();
        let targets = persist::prepare_input(&db, &account_id, &creds.account.username, &raw)
            .map(|input| report::report_targets(&input, stored.as_ref().map(|s| &s.verdict)))
            .unwrap_or_default();
        (
            stored,
            targets,
            db.get_drafts_folder(&account_id).ok().flatten(),
        )
    };
    // RDAP runs without the database lock held.
    let (abuse, lookups) = report::resolve_abuse_contacts(
        &envelope_email_transport::threat::rdap::PublicRdap,
        &targets,
    )
    .await;
    if !lookups.is_empty() {
        let db = state.db.lock().await;
        let recorded = persist::record_lookups(
            &db,
            &VerdictTarget {
                account_id: &account_id,
                folder: &q.folder,
                uid,
                message_id: stored.as_ref().and_then(|s| s.message_id.as_deref()),
            },
            &lookups,
        );
        if let Err(e) = recorded {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "audit_failed",
                format!("{e:#}"),
            );
        }
    }
    let verdict = stored.map(|s| s.verdict);
    let report = report::build_report(&raw, verdict.as_ref(), &config.report_to, &abuse);
    let built = report::report_rfc822(
        creds.account.display_name.as_deref(),
        &creds.account.username,
        &report,
    );
    let (rfc822, message_id) = match built {
        Ok(b) => b,
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "build_failed",
                format!("{e:#}"),
            );
        }
    };

    let appended = {
        let mut client = client_arc.lock().await;
        let folder = match cached_folder {
            Some(folder) => Ok(folder),
            None => envelope_email_transport::imap::drafts_special_use_folder(&mut client)
                .await
                .map(|f| f.unwrap_or_else(|| "Drafts".to_string())),
        };
        match folder {
            Ok(folder) => report::append_report_draft(&mut client, &folder, &rfc822, &message_id)
                .await
                .map(|uid| (folder, uid)),
            Err(e) => Err(anyhow::anyhow!("resolve the Drafts folder: {e}")),
        }
    };
    let (folder, imap_uid) = match appended {
        Ok(a) => a,
        Err(e) => {
            state.evict_imap(&account_id).await;
            return error(StatusCode::BAD_GATEWAY, "imap_error", format!("{e:#}"));
        }
    };
    let db = state.db.lock().await;
    match report::record_report_draft(&db, &account_id, &report, &folder, imap_uid, &message_id) {
        Ok(draft) => Json(json!({
            "status": "drafted",
            "sent": false,
            "draft_id": draft.id,
            "to": report.to,
            "abuse_contact": abuse,
            "subject": report.subject,
            "imap_folder": folder,
        }))
        .into_response(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            format!("{e:#}"),
        ),
    }
}

/// UIDs among `summaries` with no current verdict, newest first, capped.
pub fn unscanned_uids(
    db: &Database,
    account_id: &str,
    folder: &str,
    summaries: &[MessageSummary],
) -> Vec<u32> {
    let mut out: Vec<u32> = summaries
        .iter()
        .filter(|s| {
            let mid = s
                .message_id
                .as_deref()
                .map(envelope_email_store::canonical_message_id)
                .filter(|m| !m.is_empty());
            let existing = persist::latest_verdict(db, account_id, mid, folder, s.uid)
                .ok()
                .flatten();
            persist::needs_scan(existing.as_ref())
        })
        .map(|s| s.uid)
        .collect();
    out.sort_unstable_by(|a, b| b.cmp(a));
    out.truncate(SWEEP_MAX_SCANS);
    out
}

/// One sweep tick: every account whose `sync.poll_interval_secs` timer has
/// elapsed gets its newest INBOX mail scanned (read-only fetch).
pub async fn run_sweep(state: &AppState, last_run: &mut HashMap<String, Instant>) {
    let config = match ThreatConfig::load() {
        Ok(config) => config,
        Err(e) => {
            warn!("threat sweep skipped: invalid threat config: {e:#}");
            return;
        }
    };
    if !config.enabled {
        return;
    }
    let interval = Duration::from_secs(config.poll_interval_secs);
    let accounts = {
        let db = state.db.lock().await;
        match db.list_accounts() {
            Ok(accounts) => accounts,
            Err(e) => {
                warn!("threat sweep skipped: {e}");
                return;
            }
        }
    };
    for account in accounts {
        if account.imap_host.is_empty() {
            continue;
        }
        if last_run
            .get(&account.id)
            .is_some_and(|at| at.elapsed() < interval)
        {
            continue;
        }
        last_run.insert(account.id.clone(), Instant::now());
        if let Err(e) = sweep_account(state, &account, &config).await {
            warn!("threat sweep for {} failed: {e:#}", account.username);
        }
    }
}

async fn sweep_account(
    state: &AppState,
    account: &envelope_email_store::models::Account,
    config: &ThreatConfig,
) -> anyhow::Result<()> {
    const FOLDER: &str = "INBOX";
    let (client_arc, _creds) = state.get_or_create_imap(&account.id).await?;
    let summaries = {
        let mut client = client_arc.lock().await;
        envelope_email_transport::imap::fetch_folder_summaries_read_only(
            &mut client,
            FOLDER,
            SWEEP_WINDOW,
        )
        .await
    };
    let summaries = match summaries {
        Ok(s) => s,
        Err(e) => {
            state.evict_imap(&account.id).await;
            return Err(e.into());
        }
    };
    let uids = {
        let db = state.db.lock().await;
        unscanned_uids(&db, &account.id, FOLDER, &summaries)
    };
    if uids.is_empty() {
        return Ok(());
    }
    let mut client = client_arc.lock().await;
    let mut mbox = DashboardMailbox {
        state,
        client: &mut client,
        account_id: &account.id,
    };
    let run = RunAccount {
        id: &account.id,
        email: &account.username,
    };
    let results =
        persist::scan_new_mail(&mut mbox, &DashboardDb(state), &run, FOLDER, &uids, config).await;
    let mut failed = 0usize;
    for (uid, result) in results {
        match result {
            Ok(entry) => info!(
                "threat sweep {}: UID {uid} {} ({})",
                account.username,
                entry.level.as_str(),
                entry.score
            ),
            Err(e) => {
                failed += 1;
                warn!("threat sweep {}: UID {uid}: {e:#}", account.username);
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} message(s) could not be scanned");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_transport::threat::{Signal, combine};

    fn summary(uid: u32, mid: &str) -> MessageSummary {
        MessageSummary {
            uid,
            message_id: Some(format!("<{mid}>")),
            from_addr: "a@b.example".into(),
            to_addr: "me@example.org".into(),
            subject: "s".into(),
            date: None,
            flags: vec![],
            size: 0,
            provider_spam: None,
        }
    }

    #[test]
    fn sweep_scans_only_messages_without_a_current_verdict() {
        let db = Database::open_memory().unwrap();
        let verdict = combine(vec![Signal::new("x", 10, "e")], vec![], vec![], false);
        persist::record_verdict(
            &db,
            &VerdictTarget {
                account_id: "a",
                folder: "INBOX",
                uid: 2,
                message_id: Some("two@x"),
            },
            &verdict,
        )
        .unwrap();
        let mut old = verdict.clone();
        old.engine_version = "rshield-0".into();
        persist::record_verdict(
            &db,
            &VerdictTarget {
                account_id: "a",
                folder: "INBOX",
                uid: 3,
                message_id: Some("three@x"),
            },
            &old,
        )
        .unwrap();
        let uids = unscanned_uids(
            &db,
            "a",
            "INBOX",
            &[
                summary(1, "one@x"),
                summary(2, "two@x"),
                summary(3, "three@x"),
            ],
        );
        assert_eq!(
            uids,
            vec![3, 1],
            "new and stale-engine verdicts, newest first"
        );
    }
}
