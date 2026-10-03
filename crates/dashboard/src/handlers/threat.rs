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

/// The reader's view of a verdict on the message with threat `key` (see
/// [`persist::threat_key`]) and content `fingerprint`. Level and malware come
/// from the verdict itself; `marked_safe` holds only for a Mark safe bound to
/// this content. `tags` lists the `threat:*` tags stored under the key.
pub fn verdict_view(
    db: &Database,
    account_id: &str,
    key: Option<&str>,
    fingerprint: Option<&str>,
    verdict: &ThreatVerdict,
) -> anyhow::Result<Value> {
    let (tags, marked_safe) = match key {
        Some(key) => (
            db.get_tags(account_id, key)?
                .into_iter()
                .map(|t| t.tag)
                .filter(|t| t.starts_with("threat:"))
                .collect(),
            persist::is_marked_safe(db, account_id, key, fingerprint)?,
        ),
        None => (Vec::<String>::new(), false),
    };
    Ok(json!({
        "level": verdict.level,
        "score": verdict.score,
        "signals": verdict.signals,
        "explain": threat::explain(verdict),
        "engine_version": verdict.engine_version,
        "computed_at": verdict.computed_at,
        "marked_safe": marked_safe,
        "malware": verdict.is_malware(),
        "tags": tags,
    }))
}

fn stored_view(db: &Database, account_id: &str, stored: &StoredVerdict) -> anyhow::Result<Value> {
    verdict_view(
        db,
        account_id,
        stored.key.as_deref(),
        stored.content_fingerprint.as_deref(),
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
    raw: Option<&[u8]>,
    config: &ThreatConfig,
) -> anyhow::Result<Option<Value>> {
    let verdict =
        persist::verdict_on_open(db, account_id, account_address, folder, uid, raw, config)?;
    // The message's own identity: its bytes when read whole, else what the
    // verdict at this UID recorded (a message read part by part).
    let (key, fingerprint) = match raw {
        Some(raw) => {
            let fingerprint = threat::content_fingerprint(raw);
            let key = persist::threat_key(
                db,
                account_id,
                threat::sole_message_id(raw).as_deref(),
                fingerprint.as_deref(),
            )?;
            (key, fingerprint)
        }
        None => persist::stored_verdict_for_uid(db, account_id, folder, uid)?
            .map(|s| (s.key, s.content_fingerprint))
            .unwrap_or_default(),
    };
    verdict
        .map(|v| verdict_view(db, account_id, key.as_deref(), fingerprint.as_deref(), &v))
        .transpose()
}

/// `GET /api/accounts/{id}/messages/{uid}/threat` — the stored verdict only,
/// by folder/UID. A message moved back or delivered again has one under its
/// new UID once opening it has matched its bytes to a verdict.
pub async fn show(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Query(q): Query<FolderQuery>,
) -> Response {
    let db = state.db.lock().await;
    let view =
        persist::stored_verdict_for_uid(&db, &account_id, &q.folder, uid).and_then(|stored| {
            stored
                .map(|s| stored_view(&db, &account_id, &s))
                .transpose()
        });
    match view {
        Ok(view) => Json(json!({"threat": view})).into_response(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            format!("{e:#}"),
        ),
    }
}

/// `POST /api/accounts/{id}/messages/{uid}/threat/mark-safe`. Mark safe is
/// bound to the message's content, so this reads the message (read-only
/// fetch) and marks those bytes.
pub async fn mark_safe(
    State(state): State<AppState>,
    Path((account_id, uid)): Path<(String, u32)>,
    Query(q): Query<FolderQuery>,
) -> Response {
    // Nothing to override: answer before opening a mailbox connection.
    {
        let db = state.db.lock().await;
        match persist::stored_verdict_for_uid(&db, &account_id, &q.folder, uid) {
            Ok(Some(_)) => {}
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
        }
    }
    let (client_arc, _creds) = match state.get_or_create_imap(&account_id).await {
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
    let db = state.db.lock().await;
    mark_safe_message(&db, &account_id, &q.folder, uid, &raw)
}

/// Mark safe the message whose bytes are `raw` at folder/UID. The mark is
/// bound to their fingerprint and needs a stored verdict that judged them;
/// when the verdict on file is for other content the answer is
/// `rescan_required`.
pub fn mark_safe_message(
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
    raw: &[u8],
) -> Response {
    let message_id = threat::sole_message_id(raw);
    let Some(fingerprint) = threat::content_fingerprint(raw) else {
        return error(
            StatusCode::CONFLICT,
            persist::RESCAN_REQUIRED,
            "the message's content could not be fingerprinted, so it cannot be marked safe",
        );
    };
    let matched = persist::matching_verdict(
        db,
        account_id,
        folder,
        uid,
        message_id.as_deref(),
        &fingerprint,
    );
    let stored = match matched {
        Ok(Some(stored)) => stored,
        Ok(None) => {
            return error(
                StatusCode::CONFLICT,
                persist::RESCAN_REQUIRED,
                "the stored verdict is for different content; open the message to scan it",
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
    let target = VerdictTarget {
        account_id,
        folder,
        uid,
        message_id: message_id.as_deref(),
        content_fingerprint: Some(&fingerprint),
        observed_message_ids: &[],
    };
    if let Err(e) = persist::mark_safe(db, &target, "reader", None) {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            format!("{e:#}"),
        );
    }
    let view = persist::threat_key(db, account_id, message_id.as_deref(), Some(&fingerprint))
        .and_then(|key| {
            verdict_view(
                db,
                account_id,
                key.as_deref(),
                Some(&fingerprint),
                &stored.verdict,
            )
        });
    match view {
        Ok(view) => Json(json!({"status": "marked_safe", "threat": view})).into_response(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            format!("{e:#}"),
        ),
    }
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
                message_id: threat::sole_message_id(&raw).as_deref(),
                content_fingerprint: threat::content_fingerprint(&raw).as_deref(),
                observed_message_ids: &[],
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

/// UIDs among `summaries` with no current verdict, newest first, capped. A
/// verdict counts only when it was stored for the same folder/UID and is for
/// the summary's Message-ID ([`persist::StoredVerdict::is_for_message_id`]);
/// a message reusing a scanned Message-ID is scanned.
pub fn unscanned_uids(
    db: &Database,
    account_id: &str,
    folder: &str,
    summaries: &[MessageSummary],
) -> Vec<u32> {
    let mut out: Vec<u32> = summaries
        .iter()
        .filter(|s| {
            // A store error reads as "no verdict": the message is scanned.
            let existing = persist::stored_verdict_for_uid(db, account_id, folder, s.uid)
                .ok()
                .flatten()
                .filter(|stored| stored.is_for_message_id(s.message_id.as_deref()))
                .map(|stored| stored.verdict);
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
    // Its own connection, never the pooled one: a first scan fetches and
    // analyzes up to SWEEP_MAX_SCANS messages, and holding the pooled
    // connection that long starves the web UI's inbox sync on the same account.
    let (mut client, _creds) = state.connect_imap_unpooled(&account.id).await?;
    let summaries = envelope_email_transport::imap::fetch_folder_summaries_read_only(
        &mut client,
        FOLDER,
        SWEEP_WINDOW,
    )
    .await?;
    let uids = {
        let db = state.db.lock().await;
        unscanned_uids(&db, &account.id, FOLDER, &summaries)
    };
    if uids.is_empty() {
        return Ok(());
    }
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
    fn open_shows_the_malware_tag_when_the_message_has_a_new_uid() {
        // Scanned at UID 571; moved back, or delivered again, as UID 580.
        let db = Database::open_memory().unwrap();
        let raw = b"Message-ID: <phish@x>\r\nFrom: bob@example.test\r\nTo: me@example.org\r\n\
Subject: s\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
--b\r\nContent-Type: application/octet-stream\r\n\
Content-Disposition: attachment; filename=\"invoice.pdf.exe\"\r\n\r\nMZ\r\n--b--\r\n";
        let open = |uid| {
            verdict_for_open(
                &db,
                "a",
                "me@example.org",
                "INBOX",
                uid,
                Some(raw),
                &ThreatConfig::default(),
            )
            .unwrap()
            .unwrap()
        };
        let scanned = open(571);
        assert_eq!(scanned["malware"], true, "{scanned}");

        let view = open(580);

        assert_eq!(view["malware"], true, "{view}");
        assert_eq!(
            view["computed_at"], scanned["computed_at"],
            "the same bytes reuse the verdict"
        );
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
                content_fingerprint: None,
                observed_message_ids: &[],
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
                content_fingerprint: None,
                observed_message_ids: &[],
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

    fn record(db: &Database, uid: u32, message_id: &str) {
        let verdict = combine(vec![Signal::new("x", 10, "e")], vec![], vec![], false);
        let fingerprint = format!("v1:{message_id}");
        persist::record_verdict(
            db,
            &VerdictTarget {
                account_id: "a",
                folder: "INBOX",
                uid,
                message_id: Some(message_id),
                content_fingerprint: Some(&fingerprint),
                observed_message_ids: &[],
            },
            &verdict,
        )
        .unwrap();
    }

    #[test]
    fn sweep_scans_reused_message_id_at_new_uid() {
        // UID 2 was scanned; UID 5 is another message with its Message-ID.
        let db = Database::open_memory().unwrap();
        record(&db, 2, "two@x");
        let uids = unscanned_uids(
            &db,
            "a",
            "INBOX",
            &[summary(2, "two@x"), summary(5, "two@x")],
        );
        assert_eq!(uids, vec![5]);
    }

    #[test]
    fn sweep_rescans_when_slot_verdict_has_other_message_id() {
        // UID 3's verdict judged another message (the folder's UIDs were
        // reset); the Message-ID now at UID 3 was scanned at UID 7.
        let db = Database::open_memory().unwrap();
        record(&db, 3, "old@x");
        record(&db, 7, "new@x");
        let uids = unscanned_uids(&db, "a", "INBOX", &[summary(3, "new@x")]);
        assert_eq!(uids, vec![3]);
    }

    /// The sweep's mailbox: serves fixture bytes, never touches a network.
    #[derive(Default)]
    struct FixtureMailbox {
        raw: HashMap<u32, Vec<u8>>,
    }

    impl envelope_email_transport::rule_exec::RuleMailbox for FixtureMailbox {
        async fn resolve_folder(&mut self, dest: &str) -> anyhow::Result<String> {
            Ok(dest.to_string())
        }
        async fn move_message(&mut self, _: &str, _: u32, _: &str) -> anyhow::Result<()> {
            unreachable!("tag-mode quarantine never moves")
        }
        async fn set_flag(&mut self, _: &str, _: u32, _: &str) -> anyhow::Result<()> {
            unreachable!("the sweep never sets flags")
        }
        async fn remove_flag(&mut self, _: &str, _: u32, _: &str) -> anyhow::Result<()> {
            unreachable!("the sweep never removes flags")
        }
        async fn delete_message(&mut self, _: &str, _: u32) -> anyhow::Result<()> {
            unreachable!("the sweep never deletes")
        }
        async fn ensure_folder(&mut self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_unsubscribe_headers(
            &mut self,
            _: &str,
            _: u32,
        ) -> anyhow::Result<(Option<String>, Option<String>)> {
            Ok((None, None))
        }
    }

    impl persist::RawFetch for FixtureMailbox {
        async fn fetch_raw(&mut self, _: &str, uid: u32) -> anyhow::Result<Option<Vec<u8>>> {
            Ok(self.raw.get(&uid).cloned())
        }
    }

    /// One sweep scan of `uid`, as `sweep_account` runs it.
    async fn sweep_scan(db: &Database, uid: u32, raw: &[u8]) {
        let mut mbox = FixtureMailbox::default();
        mbox.raw.insert(uid, raw.to_vec());
        let account = RunAccount {
            id: "a",
            email: "me@example.org",
        };
        let results = persist::scan_new_mail(
            &mut mbox,
            db,
            &account,
            "INBOX",
            &[uid],
            &ThreatConfig::default(),
        )
        .await;
        results[0].1.as_ref().unwrap();
    }

    /// The summary the sweep reads, with the Message-ID as the server's
    /// ENVELOPE gives it.
    fn envelope_summary(uid: u32, message_id: Option<&str>) -> MessageSummary {
        MessageSummary {
            message_id: message_id.map(str::to_string),
            ..summary(uid, "unused")
        }
    }

    fn with_message_ids(headers: &str) -> Vec<u8> {
        format!(
            "From: Alice <alice@partner.example>\r\nTo: me@example.org\r\n{headers}\
             Subject: Lunch\r\n\r\nThursday?\r\n"
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn sweep_scans_a_duplicate_message_id_message_once() {
        let db = Database::open_memory().unwrap();
        let raw = with_message_ids("Message-ID: <first@x>\r\nMessage-ID: <second@x>\r\n");
        // Servers differ in which Message-ID the ENVELOPE reports.
        let summaries = [
            envelope_summary(5, Some("<first@x>")),
            envelope_summary(6, Some("<second@x>")),
        ];
        assert_eq!(unscanned_uids(&db, "a", "INBOX", &summaries), vec![6, 5]);
        sweep_scan(&db, 5, &raw).await;
        sweep_scan(&db, 6, &raw).await;
        assert!(
            unscanned_uids(&db, "a", "INBOX", &summaries).is_empty(),
            "scanned once, then skipped"
        );
    }

    #[tokio::test]
    async fn sweep_scans_an_empty_message_id_message_once() {
        let db = Database::open_memory().unwrap();
        let empty = with_message_ids("Message-ID: \r\n");
        let empty_first = with_message_ids("Message-ID:\r\nMessage-ID: <b@x>\r\n");
        let summaries = [
            envelope_summary(5, None),
            envelope_summary(6, Some("")),
            envelope_summary(7, None),
            envelope_summary(8, Some("<b@x>")),
        ];
        assert_eq!(
            unscanned_uids(&db, "a", "INBOX", &summaries),
            vec![8, 7, 6, 5]
        );
        for (uid, raw) in [
            (5, &empty),
            (6, &empty),
            (7, &empty_first),
            (8, &empty_first),
        ] {
            sweep_scan(&db, uid, raw).await;
        }
        assert!(
            unscanned_uids(&db, "a", "INBOX", &summaries).is_empty(),
            "scanned once, then skipped"
        );
    }

    #[tokio::test]
    async fn sweep_scans_a_message_whose_message_id_looks_like_a_fingerprint_key_once() {
        let db = Database::open_memory().unwrap();
        let raw = with_message_ids("Message-ID: <fp:v1:0123>\r\n");
        let summaries = [envelope_summary(5, Some("<fp:v1:0123>"))];
        assert_eq!(unscanned_uids(&db, "a", "INBOX", &summaries), vec![5]);
        sweep_scan(&db, 5, &raw).await;
        assert!(
            unscanned_uids(&db, "a", "INBOX", &summaries).is_empty(),
            "scanned once, then skipped"
        );
    }

    #[tokio::test]
    async fn sweep_rescans_a_slot_whose_observed_message_id_differs() {
        // UID 5 held a message with two Message-IDs; after a UIDVALIDITY
        // change another message sits at UID 5.
        let db = Database::open_memory().unwrap();
        let raw = with_message_ids("Message-ID: <first@x>\r\nMessage-ID: <second@x>\r\n");
        sweep_scan(&db, 5, &raw).await;
        let summaries = [envelope_summary(5, Some("<other@x>"))];
        assert_eq!(unscanned_uids(&db, "a", "INBOX", &summaries), vec![5]);
        let summaries = [envelope_summary(5, None)];
        assert_eq!(unscanned_uids(&db, "a", "INBOX", &summaries), vec![5]);
    }
}
