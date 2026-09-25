// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope threat`: scan, show, explain, mark-safe, release, report, stats.
//!
//! Reads are `EXAMINE` + `BODY.PEEK[]`; a scan never marks mail read. The
//! only mailbox mutations are the quarantine move (shipped rule, attributed
//! to `envelope:threat`) and `release`, an attributed move back. `report`
//! creates a draft and never sends.

use anyhow::{Context, Result, anyhow, bail};
use envelope_email_store::{CredentialBackend, Database, canonical_message_id};
use envelope_email_transport::imap::{self, ImapClient};
use envelope_email_transport::rule_exec::{
    ActionAttribution, ActionSource, ImapRuleMailbox, MessageTarget, RunAccount, execute_action,
};
use envelope_email_transport::rules::{Action, MessageContext};
use envelope_email_transport::threat::persist::{self, StoredVerdict, VerdictTarget};
use envelope_email_transport::threat::rdap;
use envelope_email_transport::threat::report::{self, AbuseOutcome};
use envelope_email_transport::threat::{self, TAG_QUARANTINED, ThreatConfig, ThreatInput};
use serde_json::json;

use super::common::setup_credentials;

fn load_config() -> Result<ThreatConfig> {
    ThreatConfig::load().context("threat configuration is invalid; fix it with `envelope config`")
}

fn require_enabled(config: &ThreatConfig) -> Result<()> {
    if !config.enabled {
        bail!("threat.enabled is false; run `envelope config set threat.enabled true` to scan");
    }
    Ok(())
}

fn message_id_of(raw: &[u8]) -> Option<String> {
    ThreatInput::from_raw(raw, "")
        .ok()?
        .headers
        .into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("message-id"))
        .map(|(_, v)| canonical_message_id(&v).to_string())
        .filter(|m| !m.is_empty())
}

async fn fetch_raw(client: &mut ImapClient, folder: &str, uid: u32) -> Result<Vec<u8>> {
    imap::fetch_raw_message(client, folder, uid)
        .await
        .with_context(|| format!("failed to fetch UID {uid} in {folder}"))?
        .ok_or_else(|| anyhow!("message UID {uid} not found in {folder}"))
}

/// The current verdict for a UID, scanning when none is stored or the engine
/// changed since.
async fn current_verdict(
    db: &Database,
    creds: &envelope_email_store::AccountWithCredentials,
    folder: &str,
    uid: u32,
    config: &ThreatConfig,
) -> Result<StoredVerdict> {
    let account_id = creds.account.id.as_str();
    if let Some(stored) = persist::stored_verdict_for_uid(db, account_id, folder, uid)?
        && !persist::needs_scan(Some(&stored.verdict))
    {
        return Ok(stored);
    }
    require_enabled(config)?;
    let mut client = imap::connect(creds)
        .await
        .context("IMAP connection failed")?;
    let account = RunAccount {
        id: account_id,
        email: &creds.account.username,
    };
    persist::scan_uid(&mut client, db, &account, folder, uid, config).await?;
    persist::stored_verdict_for_uid(db, account_id, folder, uid)?
        .ok_or_else(|| anyhow!("scan of UID {uid} recorded no verdict"))
}

fn tags_of(db: &Database, account_id: &str, message_id: Option<&str>) -> Result<Vec<String>> {
    let Some(mid) = message_id else {
        return Ok(Vec::new());
    };
    let mut tags: Vec<String> = db
        .get_tags(account_id, mid)?
        .into_iter()
        .map(|t| t.tag)
        .filter(|t| t.starts_with("threat:"))
        .collect();
    tags.sort();
    Ok(tags)
}

/// `threat show --json` / MCP `threat_show` payload.
pub fn verdict_json(
    db: &Database,
    account_id: &str,
    stored: &StoredVerdict,
) -> Result<serde_json::Value> {
    Ok(json!({
        "account_id": account_id,
        "folder": stored.folder,
        "uid": stored.uid,
        "message_id": stored.message_id,
        "recorded_at": stored.recorded_at,
        "tags": tags_of(db, account_id, stored.message_id.as_deref())?,
        "verdict": stored.verdict,
        "explain": threat::explain(&stored.verdict),
    }))
}

/// The verdict a `read` serves: stored, or scanned now when `threat.on_read`
/// is on and none is current.
pub(crate) fn verdict_for_read(
    db: &Database,
    creds: &envelope_email_store::AccountWithCredentials,
    folder: &str,
    uid: u32,
    raw: &[u8],
) -> Result<Option<threat::ThreatVerdict>> {
    let config = load_config()?;
    persist::verdict_on_open(
        db,
        &creds.account.id,
        &creds.account.username,
        folder,
        uid,
        raw,
        &config,
    )
}

/// Additive `read` fields: `sanitized` (true when `dangerous` HTML was
/// served through the server-side sanitizer) and `threat` (level + score,
/// or null when the message has no verdict).
pub(crate) fn apply_read_policy(
    message: &mut serde_json::Value,
    verdict: Option<&threat::ThreatVerdict>,
) {
    let dangerous = verdict.is_some_and(|v| v.level == threat::Level::Dangerous);
    let mut sanitized = false;
    if dangerous && let Some(html) = message.get("html_body").and_then(|h| h.as_str()) {
        let clean = envelope_email_transport::sanitize::sanitize_email_html(html);
        message["html_body"] = json!(clean);
        sanitized = true;
    }
    message["sanitized"] = json!(sanitized);
    message["threat"] = match verdict {
        Some(v) => json!({"level": v.level, "score": v.score, "engine_version": v.engine_version}),
        None => serde_json::Value::Null,
    };
}

#[tokio::main]
pub async fn run_scan(
    account: Option<&str>,
    folder: &str,
    limit: u32,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let config = load_config()?;
    require_enabled(&config)?;
    let (db, creds) = setup_credentials(account, backend)?;
    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;
    imap::examine_folder_info(&mut client, folder).await?;
    let mut uids = imap::list_selected_uids(&mut client).await?;
    uids.sort_unstable();
    let start = uids.len().saturating_sub(limit as usize);
    let uids = &uids[start..];

    let run = RunAccount {
        id: &creds.account.id,
        email: &creds.account.username,
    };
    let mut entries = Vec::new();
    let mut failures = 0usize;
    for &uid in uids {
        match persist::scan_uid(&mut client, &db, &run, folder, uid, &config).await {
            Ok(entry) => entries.push(json!(entry)),
            Err(e) => {
                failures += 1;
                entries.push(json!({"uid": uid, "error": format!("{e:#}")}));
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "account_id": creds.account.id,
                "folder": folder,
                "scanned": entries.len() - failures,
                "failed": failures,
                "messages": entries,
            }))?
        );
    } else {
        for e in &entries {
            match e.get("error") {
                Some(err) => println!("{:>8}  error  {err}", e["uid"]),
                None => println!(
                    "{:>8}  {:<11} {:>3}  {}",
                    e["uid"],
                    e["level"].as_str().unwrap_or_default(),
                    e["score"],
                    e["quarantine"].as_str().unwrap_or_default()
                ),
            }
        }
        println!(
            "Scanned {} message(s) in {folder}",
            entries.len() - failures
        );
    }
    if failures > 0 {
        bail!("{failures} message(s) could not be scanned");
    }
    Ok(())
}

#[tokio::main]
pub async fn run_show(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let config = load_config()?;
    let (db, creds) = setup_credentials(account, backend)?;
    let stored = current_verdict(&db, &creds, folder, uid, &config).await?;
    let value = verdict_json(&db, &creds.account.id, &stored)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        let v = &stored.verdict;
        println!(
            "UID {uid} in {folder}: {} ({}/100)",
            v.level.as_str(),
            v.score
        );
        for s in &v.signals {
            println!("  {:<24} +{:<3} {}", s.code, s.weight, s.evidence);
        }
        let tags = value["tags"].as_array().cloned().unwrap_or_default();
        if !tags.is_empty() {
            let tags: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
            println!("Tags: {}", tags.join(", "));
        }
        println!("Engine {} at {}", v.engine_version, v.computed_at);
    }
    Ok(())
}

#[tokio::main]
pub async fn run_explain(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let config = load_config()?;
    let (db, creds) = setup_credentials(account, backend)?;
    let stored = current_verdict(&db, &creds, folder, uid, &config).await?;
    let lines = threat::explain(&stored.verdict);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "uid": uid,
                "folder": folder,
                "score": stored.verdict.score,
                "level": stored.verdict.level,
                "lines": lines,
            }))?
        );
    } else {
        for line in lines {
            println!("{line}");
        }
    }
    Ok(())
}

#[tokio::main]
pub async fn run_mark_safe(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = creds.account.id.clone();
    let recorded =
        persist::stored_verdict_for_uid(&db, &account_id, folder, uid)?.and_then(|s| s.message_id);
    let message_id = match recorded {
        Some(mid) => mid,
        None => {
            let mut client = imap::connect(&creds)
                .await
                .context("IMAP connection failed")?;
            message_id_of(&fetch_raw(&mut client, folder, uid).await?)
                .ok_or_else(|| anyhow!("UID {uid} in {folder} has no Message-ID to tag"))?
        }
    };
    persist::mark_safe(
        &db,
        &VerdictTarget {
            account_id: &account_id,
            folder,
            uid,
            message_id: Some(&message_id),
        },
        "cli",
        None,
    )?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "status": "marked_safe",
                "uid": uid,
                "folder": folder,
                "message_id": message_id,
                "tag": threat::TAG_FALSE_POSITIVE,
            }))?
        );
    } else {
        println!(
            "Marked UID {uid} in {folder} safe ({})",
            threat::TAG_FALSE_POSITIVE
        );
    }
    Ok(())
}

#[tokio::main]
pub async fn run_release(
    uid: u32,
    folder: &str,
    to: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = creds.account.id.clone();
    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;
    let message_id = message_id_of(&fetch_raw(&mut client, folder, uid).await?);

    let ctx = MessageContext {
        from_addr: String::new(),
        to_addr: String::new(),
        subject: String::new(),
        tags: Vec::new(),
        scores: Default::default(),
        contact_tags: Vec::new(),
    };
    let target = MessageTarget {
        account_id: &account_id,
        account_email: &creds.account.username,
        folder,
        uid,
        message_id: message_id.as_deref(),
        ctx: &ctx,
    };
    let mut mbox = ImapRuleMailbox {
        client: &mut client,
        db: &db,
        account_id: &account_id,
    };
    let outcome = execute_action(
        &mut mbox,
        &db,
        &target,
        &Action::Move(to.to_string()),
        None,
        &ActionAttribution::new(ActionSource::Cli),
    )
    .await
    .with_context(|| format!("release of UID {uid} from {folder} failed"))?;
    if let Some(mid) = message_id.as_deref() {
        db.remove_tag(&account_id, mid, TAG_QUARANTINED)?;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "status": "released",
                "uid": uid,
                "from": folder,
                "result": outcome.description,
                "message_id": message_id,
            }))?
        );
    } else {
        println!("Released UID {uid} from {folder}: {}", outcome.description);
    }
    Ok(())
}

#[tokio::main]
pub async fn run_report(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let config = load_config()?;
    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = creds.account.id.clone();
    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;
    let raw = fetch_raw(&mut client, folder, uid).await?;
    drop(client);

    let message_id = message_id_of(&raw);
    let target = VerdictTarget {
        account_id: &account_id,
        folder,
        uid,
        message_id: message_id.as_deref(),
    };
    let verdict = match persist::stored_verdict_for_uid(&db, &account_id, folder, uid)? {
        Some(stored) => Some(stored.verdict),
        None if config.enabled => {
            let (verdict, scanned) =
                persist::scan_raw(&db, &account_id, &creds.account.username, &raw, &config);
            persist::record_verdict(&db, &target, &verdict)?;
            persist::record_lookups(&db, &target, &scanned.lookups)?;
            Some(verdict)
        }
        None => None,
    };
    let targets = persist::prepare_input(&db, &account_id, &creds.account.username, &raw)
        .map(|input| report::report_targets(&input, verdict.as_ref()))
        .unwrap_or_default();
    let (abuse, lookups) = report::resolve_abuse_contacts(&rdap::PublicRdap, &targets).await;
    persist::record_lookups(&db, &target, &lookups)?;
    let report = report::build_report(&raw, verdict.as_ref(), &config.report_to, &abuse);
    let (draft, drafts_folder, imap_uid) =
        super::drafts::create_threat_report_draft(&db, &creds, &report).await?;
    let review_url = super::drafts::draft_dashboard_url(&account_id, &draft.id);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "status": "drafted",
                "sent": false,
                "draft_id": draft.id,
                "to": report.to,
                "abuse_contact": abuse,
                "subject": report.subject,
                "imap_folder": drafts_folder,
                "imap_uid": imap_uid,
                "review_url": review_url,
                "next": format!("review, then `envelope draft send {}` (Governor-gated)", draft.id),
            }))?
        );
    } else {
        println!("Report draft created (not sent): {}", draft.id);
        println!("  To:      {}", report.to);
        for contact in &abuse {
            let role = contact.role.as_str();
            let domain = &contact.domain;
            match &contact.outcome {
                AbuseOutcome::Found { email } => {
                    println!("  Abuse:   {email} (RDAP, {role} domain {domain})")
                }
                AbuseOutcome::Failed { reason } => println!(
                    "  Abuse:   no abuse contact for {role} domain {domain} ({reason}); left out"
                ),
            }
        }
        println!("  Subject: {}", report.subject);
        println!("  Review:  {review_url}");
        println!(
            "Send it with `envelope draft send {}` after review.",
            draft.id
        );
    }
    Ok(())
}

/// Local read only: no credentials, no IMAP.
pub fn run_stats(account: Option<&str>, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let account = super::common::resolve_account(&db, account)?;
    let stats = persist::stats(&db, &account.id)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "account_id": account.id,
                "stats": stats,
            }))?
        );
    } else {
        println!("Scanned messages: {}", stats.scanned);
        println!(
            "  clean {}  suspicious {}  dangerous {}  unavailable {}  (malware {})",
            stats.clean, stats.suspicious, stats.dangerous, stats.unavailable, stats.malware
        );
        println!(
            "  non-clean rate {:.1}%  unavailable rate {:.1}%",
            stats.non_clean_rate * 100.0,
            stats.unavailable_rate * 100.0
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_policy_sanitizes_only_dangerous_html() {
        let v = |score| {
            threat::combine(
                vec![threat::Signal::new("x", score, "e")],
                vec![],
                vec![],
                false,
            )
        };
        let html = r#"<p>hi</p><script>evil()</script><a href="javascript:x()">l</a>"#;

        let mut msg = json!({"html_body": html, "text_body": "hi"});
        apply_read_policy(&mut msg, Some(&v(80)));
        assert_eq!(msg["sanitized"], true);
        assert_eq!(msg["threat"]["level"], "dangerous");
        let served = msg["html_body"].as_str().unwrap();
        assert!(served.contains("<p>hi</p>"));
        assert!(!served.contains("script") && !served.contains("javascript"));

        let mut msg = json!({"html_body": html});
        apply_read_policy(&mut msg, Some(&v(40)));
        assert_eq!(msg["sanitized"], false);
        assert_eq!(msg["html_body"], html, "suspicious HTML is served as-is");

        let mut msg = json!({"html_body": null});
        apply_read_policy(&mut msg, None);
        assert_eq!(msg["sanitized"], false);
        assert!(msg["threat"].is_null());
    }

    #[test]
    fn message_id_is_read_from_raw_headers() {
        assert_eq!(
            message_id_of(b"Message-ID: <abc@host>\r\nFrom: a@b\r\n\r\nx").as_deref(),
            Some("abc@host")
        );
        assert_eq!(message_id_of(b"From: a@b\r\n\r\nx"), None);
    }
}
