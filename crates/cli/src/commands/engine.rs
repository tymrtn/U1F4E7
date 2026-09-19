// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use envelope_email_store::credential_store::{self, CredentialBackend};
use envelope_email_store::models::{Account, Event, Message};
use envelope_email_store::{
    Database, MAIL_ENGINE_SCHEMA_VERSION, MailEngineDecisionClaim, MailEngineDigestCandidate,
    MailEngineSenderStats, MailboxScanPlan, NewMailEngineDecision, mail_engine_hash,
};
use envelope_email_transport::folders;
use envelope_email_transport::imap;
use envelope_email_transport::jev::{
    JevClient, JevState, MailRoute, MessageFlags, PastInteractions, PolicyDecision, ReplyHistory,
    SenderState, SenderStatistics, Urgency, ValidatedDecision, apply_policy, build_request,
};
use serde::Serialize;

use super::{common::resolve_account, provenance};

const MAX_MESSAGES_PER_PASS: usize = 100;

#[derive(Debug, Clone)]
pub struct EngineOptions<'a> {
    pub account: Option<&'a str>,
    pub folder: &'a str,
    pub apply: bool,
    pub json: bool,
    pub backend: CredentialBackend,
}

#[derive(Debug, Serialize)]
struct EnginePassReport {
    mode: &'static str,
    interval_seconds: Option<u64>,
    apply: bool,
    accounts: Vec<AccountReport>,
}

#[derive(Debug, Serialize)]
struct AccountReport {
    account_id: String,
    status: String,
    baseline_uid: Option<u32>,
    examined: usize,
    decided: usize,
    review: usize,
    actions_completed: usize,
    actions_blocked: usize,
    error_code: Option<String>,
}

#[derive(Debug, Serialize)]
struct DigestPreviewReport {
    items: Vec<DigestPreviewItem>,
    errors: Vec<DigestPreviewError>,
}

#[derive(Debug, Serialize)]
struct DigestPreviewItem {
    account_id: String,
    folder: String,
    uidvalidity: u32,
    uid: u32,
    route_probability: Option<f64>,
    route_confidence: Option<f64>,
    urgency: String,
    decided_at: String,
    trust: serde_json::Value,
    untrusted_content: DigestPreviewContent,
}

#[derive(Debug, Serialize)]
struct DigestPreviewContent {
    from: Option<String>,
    subject: Option<String>,
    date: Option<String>,
}

#[derive(Debug, Serialize)]
struct DigestPreviewError {
    account_id: String,
    folder: String,
    uidvalidity: u32,
    uid: Option<u32>,
    error_code: &'static str,
}

impl AccountReport {
    fn new(account_id: String) -> Self {
        Self {
            account_id,
            status: "ok".into(),
            baseline_uid: None,
            examined: 0,
            decided: 0,
            review: 0,
            actions_completed: 0,
            actions_blocked: 0,
            error_code: None,
        }
    }
}

#[tokio::main]
pub async fn run_once(options: EngineOptions<'_>) -> Result<()> {
    let accounts = process_once(&options).await?;
    print_report(
        &EnginePassReport {
            mode: "once",
            interval_seconds: None,
            apply: options.apply,
            accounts,
        },
        options.json,
    )
}

#[tokio::main]
pub async fn run_loop(options: EngineOptions<'_>, interval_seconds: u64) -> Result<()> {
    if interval_seconds < 60 {
        bail!("engine interval must be at least 60 seconds");
    }
    loop {
        let accounts = process_once(&options).await?;
        print_report(
            &EnginePassReport {
                mode: "run",
                interval_seconds: Some(interval_seconds),
                apply: options.apply,
                accounts,
            },
            options.json,
        )?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_secs(interval_seconds)) => {}
        }
    }
    Ok(())
}

pub fn run_status(account: Option<&str>, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let account_id = account
        .map(|value| resolve_account(&db, Some(value)).map(|account| account.id))
        .transpose()?;
    let status = db.list_mail_engine_status(account_id.as_deref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else if status.is_empty() {
        println!("Mail engine has not established a mailbox baseline yet.");
    } else {
        for item in status {
            println!(
                "{} {}: UID {} ({} decisions, {} review, {} follow-up, {} important, {} digest, {} unsubscribe candidates)",
                item.account_id,
                item.folder,
                item.last_seen_uid,
                item.decision_count,
                item.review_count,
                item.follow_up_count,
                item.important_count,
                item.digest_news_count,
                item.unsubscribe_candidate_count,
            );
        }
    }
    Ok(())
}

pub fn run_digest_queue(account: Option<&str>, limit: usize, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let account_id = account
        .map(|value| resolve_account(&db, Some(value)).map(|account| account.id))
        .transpose()?;
    let queue = db.list_mail_engine_digest_queue(account_id.as_deref(), limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&queue)?);
    } else if queue.is_empty() {
        println!("The Jev news-digest queue is empty.");
    } else {
        for item in queue {
            println!(
                "{} {} UID {} (UIDVALIDITY {}, route probability {}, confidence {}, decided {})",
                item.account_id,
                item.folder,
                item.uid,
                item.uidvalidity,
                display_probability(item.route_probability),
                display_probability(item.route_confidence),
                item.decided_at,
            );
        }
    }
    Ok(())
}

#[tokio::main]
pub async fn run_digest_preview(
    account: Option<&str>,
    limit: usize,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let account_id = account
        .map(|value| resolve_account(&db, Some(value)).map(|account| account.id))
        .transpose()?;
    let queue = db.list_mail_engine_digest_queue(account_id.as_deref(), limit)?;
    if queue.is_empty() {
        return print_digest_preview(
            &DigestPreviewReport {
                items: Vec::new(),
                errors: Vec::new(),
            },
            json,
        );
    }

    let passphrase = credential_store::get_passphrase(backend)
        .context("credential store is not available for digest preview")?;
    let mut groups: BTreeMap<(String, String, u32), Vec<MailEngineDigestCandidate>> =
        BTreeMap::new();
    for candidate in queue {
        groups
            .entry((
                candidate.account_id.clone(),
                candidate.folder.clone(),
                candidate.uidvalidity,
            ))
            .or_default()
            .push(candidate);
    }

    let mut report = DigestPreviewReport {
        items: Vec::new(),
        errors: Vec::new(),
    };
    for ((account_id, folder, uidvalidity), candidates) in groups {
        let credentials = match db.get_account_with_credentials(&account_id, &passphrase) {
            Ok(credentials) => credentials,
            Err(_) => {
                report.errors.push(DigestPreviewError {
                    account_id,
                    folder,
                    uidvalidity,
                    uid: None,
                    error_code: "credential_decrypt_failed",
                });
                continue;
            }
        };
        let mut client = match imap::connect(&credentials).await {
            Ok(client) => client,
            Err(_) => {
                report.errors.push(DigestPreviewError {
                    account_id,
                    folder,
                    uidvalidity,
                    uid: None,
                    error_code: "imap_connect_failed",
                });
                continue;
            }
        };
        let selected = match imap::examine_folder_info(&mut client, &folder).await {
            Ok(selected) => selected,
            Err(_) => {
                report.errors.push(DigestPreviewError {
                    account_id,
                    folder,
                    uidvalidity,
                    uid: None,
                    error_code: "imap_examine_failed",
                });
                continue;
            }
        };
        if selected.uid_validity != Some(uidvalidity) {
            report.errors.push(DigestPreviewError {
                account_id,
                folder,
                uidvalidity,
                uid: None,
                error_code: "uidvalidity_changed",
            });
            continue;
        }

        let uid_set = candidates
            .iter()
            .map(|candidate| candidate.uid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let summaries =
            match imap::fetch_message_peek_headers_selected_uid_set(&mut client, &folder, &uid_set)
                .await
            {
                Ok(summaries) => summaries,
                Err(_) => {
                    report.errors.push(DigestPreviewError {
                        account_id,
                        folder,
                        uidvalidity,
                        uid: None,
                        error_code: "message_fetch_failed",
                    });
                    continue;
                }
            };
        let by_uid: HashMap<u32, imap::PeekHeaderSummary> = summaries
            .into_iter()
            .map(|summary| (summary.uid, summary))
            .collect();
        for candidate in candidates {
            if let Some(summary) = by_uid.get(&candidate.uid) {
                report
                    .items
                    .push(compile_digest_preview_item(&candidate, summary));
            } else {
                report.errors.push(DigestPreviewError {
                    account_id: candidate.account_id,
                    folder: candidate.folder,
                    uidvalidity: candidate.uidvalidity,
                    uid: Some(candidate.uid),
                    error_code: "message_missing",
                });
            }
        }
    }
    report
        .items
        .sort_by(|left, right| right.decided_at.cmp(&left.decided_at));
    print_digest_preview(&report, json)
}

fn compile_digest_preview_item(
    candidate: &MailEngineDigestCandidate,
    summary: &imap::PeekHeaderSummary,
) -> DigestPreviewItem {
    DigestPreviewItem {
        account_id: candidate.account_id.clone(),
        folder: candidate.folder.clone(),
        uidvalidity: candidate.uidvalidity,
        uid: candidate.uid,
        route_probability: candidate.route_probability,
        route_confidence: candidate.route_confidence,
        urgency: candidate.urgency.clone(),
        decided_at: candidate.decided_at.clone(),
        trust: provenance::inbound_trust(),
        untrusted_content: DigestPreviewContent {
            from: summary
                .from_addr
                .as_deref()
                .map(|value| digest_header_text(value, 320)),
            subject: summary
                .subject
                .as_deref()
                .map(|value| digest_header_text(value, 512)),
            date: summary.date.clone(),
        },
    }
}

fn digest_header_text(value: &str, max_chars: usize) -> String {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    normalized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn print_digest_preview(report: &DigestPreviewReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else if report.items.is_empty() && report.errors.is_empty() {
        println!("The Jev news-digest queue is empty.");
    } else {
        if !report.items.is_empty() {
            println!("{}", provenance::INBOUND_WARNING);
        }
        for item in &report.items {
            println!(
                "- {} — {} ({}, {} {} UID {})",
                item.untrusted_content
                    .subject
                    .as_deref()
                    .unwrap_or("subject unavailable"),
                item.untrusted_content
                    .from
                    .as_deref()
                    .unwrap_or("sender unavailable"),
                item.untrusted_content
                    .date
                    .as_deref()
                    .unwrap_or("date unknown"),
                item.account_id,
                item.folder,
                item.uid,
            );
        }
        for error in &report.errors {
            println!(
                "! {} {} UID {}: {}",
                error.account_id,
                error.folder,
                error
                    .uid
                    .map(|uid| uid.to_string())
                    .unwrap_or_else(|| "-".into()),
                error.error_code,
            );
        }
    }
    Ok(())
}

fn display_probability(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.3}"))
        .unwrap_or_else(|| "unknown".into())
}

async fn process_once(options: &EngineOptions<'_>) -> Result<Vec<AccountReport>> {
    let db = Database::open_default().context("failed to open database")?;
    let passphrase = credential_store::get_or_create_passphrase(options.backend)
        .context("credential store error")?;
    let accounts = selected_accounts(&db, options.account)?;
    let mut reports = Vec::with_capacity(accounts.len());

    for account in accounts {
        let mut report = AccountReport::new(account.id.clone());
        let credentials = match db.get_account_with_credentials(&account.id, &passphrase) {
            Ok(credentials) => credentials,
            Err(_) => {
                report.status = "failed".into();
                report.error_code = Some("credential_decrypt_failed".into());
                reports.push(report);
                continue;
            }
        };
        let mut client = match imap::connect(&credentials).await {
            Ok(client) => client,
            Err(_) => {
                report.status = "failed".into();
                report.error_code = Some("imap_connect_failed".into());
                reports.push(report);
                continue;
            }
        };
        let selected = match imap::examine_folder_info(&mut client, options.folder).await {
            Ok(selected) => selected,
            Err(_) => {
                report.status = "failed".into();
                report.error_code = Some("imap_examine_failed".into());
                reports.push(report);
                continue;
            }
        };
        let Some(uidvalidity) = selected.uid_validity else {
            report.status = "held".into();
            report.error_code = Some("uidvalidity_missing".into());
            reports.push(report);
            continue;
        };
        let highest_uid = match selected.last_uid() {
            Some(uid) => uid,
            None => match imap::list_selected_uids(&mut client).await {
                Ok(uids) => uids.into_iter().max().unwrap_or(0),
                Err(_) => {
                    report.status = "held".into();
                    report.error_code = Some("highest_uid_unavailable".into());
                    reports.push(report);
                    continue;
                }
            },
        };
        let scan_plan =
            db.plan_mail_engine_scan(&account.id, options.folder, uidvalidity, highest_uid)?;
        if options.apply {
            for pending_uid in db.list_pending_mail_engine_junk(
                &account.id,
                options.folder,
                uidvalidity,
                MAX_MESSAGES_PER_PASS,
            )? {
                execute_junk(
                    &db,
                    &mut client,
                    &account.id,
                    options.folder,
                    uidvalidity,
                    pending_uid,
                    &mut report,
                )
                .await;
                let _ = imap::examine_folder_info(&mut client, options.folder).await;
            }
        }
        match scan_plan {
            MailboxScanPlan::Baseline { last_seen_uid, .. } => {
                report.status = "baselined".into();
                report.baseline_uid = Some(last_seen_uid);
                reports.push(report);
                continue;
            }
            MailboxScanPlan::Rebaseline { last_seen_uid, .. } => {
                report.status = "rebaselined".into();
                report.baseline_uid = Some(last_seen_uid);
                report.error_code = Some("uidvalidity_changed".into());
                reports.push(report);
                continue;
            }
            MailboxScanPlan::Current => {
                report.status = "current".into();
                reports.push(report);
                continue;
            }
            MailboxScanPlan::NewRange {
                after_uid,
                through_uid,
                ..
            } => {
                let mut uids = imap::list_selected_uids(&mut client).await?;
                uids.retain(|uid| *uid > after_uid && *uid <= through_uid);
                uids.sort_unstable();
                uids.truncate(MAX_MESSAGES_PER_PASS);
                if uids.is_empty() {
                    // The UID gap contains only messages removed by another
                    // client. Advancing cannot skip a message that still exists.
                    db.advance_mail_engine_watermark(
                        &account.id,
                        options.folder,
                        uidvalidity,
                        through_uid,
                    )?;
                    report.status = "current".into();
                }
                for uid in uids {
                    report.examined += 1;
                    if let Some((route, execution_status)) = db.get_mail_engine_decision_execution(
                        &account.id,
                        options.folder,
                        uidvalidity,
                        uid,
                    )? {
                        if options.apply && route == "junk" && execution_status == "pending" {
                            execute_junk(
                                &db,
                                &mut client,
                                &account.id,
                                options.folder,
                                uidvalidity,
                                uid,
                                &mut report,
                            )
                            .await;
                            let _ = imap::examine_folder_info(&mut client, options.folder).await;
                        }
                        db.advance_mail_engine_watermark(
                            &account.id,
                            options.folder,
                            uidvalidity,
                            uid,
                        )?;
                        continue;
                    }

                    let raw = match imap::fetch_raw_messages_selected_uid_set(
                        &mut client,
                        options.folder,
                        &uid.to_string(),
                    )
                    .await
                    {
                        Ok(mut messages) if messages.len() == 1 => messages.remove(0),
                        _ => {
                            persist_review(
                                &db,
                                &account.id,
                                options.folder,
                                uidvalidity,
                                uid,
                                "message_fetch_failed",
                            )?;
                            report.review += 1;
                            db.advance_mail_engine_watermark(
                                &account.id,
                                options.folder,
                                uidvalidity,
                                uid,
                            )?;
                            continue;
                        }
                    };
                    let message = match imap::parse_raw_message(&raw) {
                        Ok(message) => message,
                        Err(_) => {
                            persist_review(
                                &db,
                                &account.id,
                                options.folder,
                                uidvalidity,
                                uid,
                                "message_parse_failed",
                            )?;
                            report.review += 1;
                            db.advance_mail_engine_watermark(
                                &account.id,
                                options.folder,
                                uidvalidity,
                                uid,
                            )?;
                            continue;
                        }
                    };
                    let policy = classify_and_persist(
                        &db,
                        &account.id,
                        options.folder,
                        uidvalidity,
                        &message,
                        None,
                    )
                    .await?;
                    if policy.route == MailRoute::Review {
                        report.review += 1;
                    } else {
                        report.decided += 1;
                    }
                    if policy.notify_user_now {
                        persist_urgent_event(
                            &db,
                            &account.id,
                            options.folder,
                            uidvalidity,
                            uid,
                            policy.urgency,
                        );
                    }
                    if options.apply && policy.route == MailRoute::Junk {
                        execute_junk(
                            &db,
                            &mut client,
                            &account.id,
                            options.folder,
                            uidvalidity,
                            uid,
                            &mut report,
                        )
                        .await;
                        let _ = imap::examine_folder_info(&mut client, options.folder).await;
                    }
                    db.advance_mail_engine_watermark(
                        &account.id,
                        options.folder,
                        uidvalidity,
                        uid,
                    )?;
                }
            }
        }
        reports.push(report);
    }
    Ok(reports)
}

fn selected_accounts(db: &Database, account: Option<&str>) -> Result<Vec<Account>> {
    match account {
        Some(value) => Ok(vec![resolve_account(db, Some(value))?]),
        None => db.list_accounts().context("failed to list accounts"),
    }
}

async fn classify_and_persist(
    db: &Database,
    account_id: &str,
    folder: &str,
    uidvalidity: u32,
    message: &Message,
    client_override: Option<&JevClient>,
) -> Result<PolicyDecision> {
    let sender_address = message.from_addr.trim();
    if sender_address.is_empty() {
        persist_review(
            db,
            account_id,
            folder,
            uidvalidity,
            message.uid,
            "sender_missing",
        )?;
        return Ok(review_policy());
    }
    let stats = db
        .derive_mail_engine_sender_stats(account_id, sender_address)
        .unwrap_or_else(|_| MailEngineSenderStats {
            source_version: MAIL_ENGINE_SCHEMA_VERSION,
            ..Default::default()
        });
    let domain = sender_address
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim_end_matches('>').to_ascii_lowercase())
        .unwrap_or_default();
    db.upsert_mail_engine_sender_stats(account_id, sender_address, &domain, &stats)?;
    let flags = message_flags(&message.flags);
    let sender = SenderState {
        address: sender_address.to_ascii_lowercase(),
        domain,
        statistics: SenderStatistics {
            total_received: stats.total_received,
            read_count: stats.read_count,
            unread_count: stats.unread_count,
            junk_count: stats.junk_count,
            replied_thread_count: stats.replied_thread_count,
            outbound_count: stats.outbound_count,
            inbound_count: stats.inbound_count,
            distinct_thread_count: stats.distinct_thread_count,
            first_seen: stats.first_seen.clone(),
            last_seen: stats.last_seen.clone(),
        },
        past_interactions: PastInteractions {
            has_received_before: stats.inbound_count > 0,
            has_sent_to_sender: stats.outbound_count > 0,
            bilateral_history: stats.inbound_count > 0 && stats.outbound_count > 0,
        },
        reply_history: ReplyHistory {
            has_replied_to_sender: stats.replied_thread_count > 0,
            sender_has_replied: stats.replied_thread_count > 0,
            replied_thread_count: stats.replied_thread_count,
        },
        history_complete: stats.history_complete,
        history_source_version: stats.source_version,
    };
    let decision_text = envelope_email_transport::compose::message_preview_source(message);
    let state = JevState::new(
        message.subject.clone(),
        &decision_text,
        message.date.clone(),
        flags,
        !message.attachments.is_empty(),
        sender,
    )?;
    let request = build_request(state);
    let input_hash = mail_engine_hash(&serde_json::to_string(&request)?);
    if !db.claim_mail_engine_decision(&MailEngineDecisionClaim {
        account_id,
        folder,
        uidvalidity,
        uid: message.uid,
        input_hash: &input_hash,
        model: "typesafe/jev-1.13",
    })? {
        // Another worker already owns or completed this immutable input. Never
        // issue a second paid request; the durable row is the authority.
        return Ok(review_policy());
    }
    let owned_client = if client_override.is_none() {
        let client_result = match std::env::var("OPENROUTER_API_KEY") {
            Ok(key) if !key.trim().is_empty() => JevClient::openrouter(key),
            _ => Err(envelope_email_transport::jev::JevClientError::MissingApiKey),
        };
        match client_result {
            Ok(client) => Some(client),
            Err(_) => {
                persist_review_with_hash(
                    db,
                    account_id,
                    folder,
                    uidvalidity,
                    message.uid,
                    &input_hash,
                    "jev_request_failed",
                )?;
                return Ok(review_policy());
            }
        }
    } else {
        None
    };
    let client = client_override
        .or(owned_client.as_ref())
        .expect("Jev client exists");
    let decision = match client.decide(&request).await {
        Ok(decision) => decision,
        Err(_) => {
            persist_review_with_hash(
                db,
                account_id,
                folder,
                uidvalidity,
                message.uid,
                &input_hash,
                "jev_request_failed",
            )?;
            return Ok(review_policy());
        }
    };
    let policy = apply_policy(&decision);
    persist_decision(
        db,
        account_id,
        folder,
        uidvalidity,
        message.uid,
        &input_hash,
        &decision,
        &policy,
    )?;
    Ok(policy)
}

fn persist_decision(
    db: &Database,
    account_id: &str,
    folder: &str,
    uidvalidity: u32,
    uid: u32,
    input_hash: &str,
    decision: &ValidatedDecision,
    policy: &PolicyDecision,
) -> Result<()> {
    let decision_json = serde_json::to_string(decision)?;
    let finalized = db.finalize_mail_engine_decision(&NewMailEngineDecision {
        account_id,
        folder,
        uidvalidity,
        uid,
        input_hash,
        model: &decision.model,
        status: if policy.route == MailRoute::Review {
            "review"
        } else {
            "decided"
        },
        route: route_token(policy.route),
        route_probability: Some(decision.route_probability),
        route_confidence: Some(decision.route_confidence),
        urgency: urgency_token(policy.urgency),
        notify_user_probability: Some(decision.notify_user_probability),
        requires_reply_probability: Some(decision.requires_reply_probability),
        bulk_or_subscription_probability: Some(decision.bulk_or_subscription_probability),
        decision_json: &decision_json,
    })?;
    if !finalized {
        bail!("mail engine decision claim was not available for finalization");
    }
    Ok(())
}

fn persist_review(
    db: &Database,
    account_id: &str,
    folder: &str,
    uidvalidity: u32,
    uid: u32,
    error_code: &str,
) -> Result<()> {
    let input_hash = mail_engine_hash(&format!("{account_id}:{folder}:{uidvalidity}:{uid}"));
    persist_review_with_hash(
        db,
        account_id,
        folder,
        uidvalidity,
        uid,
        &input_hash,
        error_code,
    )
}

#[allow(clippy::too_many_arguments)]
fn persist_review_with_hash(
    db: &Database,
    account_id: &str,
    folder: &str,
    uidvalidity: u32,
    uid: u32,
    input_hash: &str,
    error_code: &str,
) -> Result<()> {
    let decision_json = serde_json::json!({"error_code": error_code}).to_string();
    let decision = NewMailEngineDecision {
        account_id,
        folder,
        uidvalidity,
        uid,
        input_hash,
        model: "typesafe/jev-1.13",
        status: "review",
        route: "review",
        route_probability: None,
        route_confidence: None,
        urgency: "not_urgent",
        notify_user_probability: None,
        requires_reply_probability: None,
        bulk_or_subscription_probability: None,
        decision_json: &decision_json,
    };
    if !db.finalize_mail_engine_decision(&decision)? {
        db.insert_mail_engine_decision_if_absent(&decision)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn execute_junk(
    db: &Database,
    client: &mut imap::ImapClient,
    account_id: &str,
    folder: &str,
    uidvalidity: u32,
    uid: u32,
    report: &mut AccountReport,
) {
    match db.claim_mail_engine_execution(account_id, folder, uidvalidity, uid) {
        Ok(true) => {}
        Ok(false) => return,
        Err(_) => {
            report.actions_blocked += 1;
            return;
        }
    }
    let destination = folders::detect_folder(client, db, account_id, "spam").await;
    match destination {
        Ok(Some(destination)) => {
            match imap::move_message_uidplus_required(
                client,
                uid,
                folder,
                &destination,
                uidvalidity,
            )
            .await
            {
                Ok(()) => {
                    let _ = db.set_mail_engine_execution(
                        account_id,
                        folder,
                        uidvalidity,
                        uid,
                        "completed",
                        Some("move_to_spam"),
                        None,
                    );
                    report.actions_completed += 1;
                }
                Err(_) => {
                    let _ = db.set_mail_engine_execution(
                        account_id,
                        folder,
                        uidvalidity,
                        uid,
                        "failed",
                        Some("move_to_spam"),
                        Some("imap_move_failed"),
                    );
                    report.actions_blocked += 1;
                }
            }
        }
        _ => {
            let _ = db.set_mail_engine_execution(
                account_id,
                folder,
                uidvalidity,
                uid,
                "blocked",
                Some("move_to_spam"),
                Some("spam_folder_not_found"),
            );
            report.actions_blocked += 1;
        }
    }
}

fn persist_urgent_event(
    db: &Database,
    account_id: &str,
    folder: &str,
    uidvalidity: u32,
    uid: u32,
    urgency: Urgency,
) {
    let marker = mail_engine_hash(&format!(
        "{account_id}:{folder}:{uidvalidity}:{uid}:mail_engine_urgent"
    ));
    let event = Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: account_id.to_string(),
        event_type: "mail_engine_urgent".into(),
        folder: folder.to_string(),
        uid: Some(i64::from(uid)),
        message_id: None,
        from_addr: None,
        subject: None,
        snippet: None,
        payload: Some(
            serde_json::json!({"urgency": urgency_token(urgency), "source": "jev"}).to_string(),
        ),
        idempotency_key: Some(marker),
        secure_pending: false,
        acked_at: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let _ = db.insert_event_idempotent(&event);
}

fn message_flags(flags: &[String]) -> MessageFlags {
    let read = flags
        .iter()
        .any(|flag| flag.eq_ignore_ascii_case("Seen") || flag.eq_ignore_ascii_case("\\Seen"));
    let junk = flags.iter().any(|flag| {
        matches!(
            flag.to_ascii_lowercase().as_str(),
            "junk" | "\\junk" | "$junk" | "custom(\"junk\")" | "custom(\"$junk\")"
        )
    });
    MessageFlags {
        read,
        unread: !read,
        junk,
    }
}

fn review_policy() -> PolicyDecision {
    PolicyDecision {
        route: MailRoute::Review,
        urgency: Urgency::NotUrgent,
        notify_user_now: false,
        requires_reply: false,
        bulk_or_subscription: false,
        abstained: true,
    }
}

fn route_token(route: MailRoute) -> &'static str {
    match route {
        MailRoute::Junk => "junk",
        MailRoute::FollowUp => "follow_up",
        MailRoute::Important => "important",
        MailRoute::Routine => "routine",
        MailRoute::DigestNews => "digest_news",
        MailRoute::UnsubscribeCandidate => "unsubscribe_candidate",
        MailRoute::Review => "review",
    }
}

fn urgency_token(urgency: Urgency) -> &'static str {
    match urgency {
        Urgency::NotUrgent => "not_urgent",
        Urgency::Urgent => "urgent",
        Urgency::Critical => "critical",
    }
}

fn print_report(report: &EnginePassReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(report)?);
    } else {
        for account in &report.accounts {
            println!(
                "{}: {} — {} examined, {} decided, {} review, {} actions completed, {} blocked",
                account.account_id,
                account.status,
                account.examined,
                account.decided,
                account.review,
                account.actions_completed,
                account.actions_blocked,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_flags_are_explicit_and_complementary() {
        let unread = message_flags(&[]);
        assert!(!unread.read);
        assert!(unread.unread);
        assert!(!unread.junk);

        let read_junk = message_flags(&["Seen".into(), "Custom(\"$Junk\")".into()]);
        assert!(read_junk.read);
        assert!(!read_junk.unread);
        assert!(read_junk.junk);
    }

    #[test]
    fn digest_preview_uses_only_envelope_fields_needed_by_the_digest() {
        let candidate = MailEngineDigestCandidate {
            account_id: "acct".into(),
            folder: "INBOX".into(),
            uidvalidity: 10,
            uid: 42,
            route_probability: Some(0.91),
            route_confidence: Some(0.88),
            urgency: "not_urgent".into(),
            requires_reply_probability: Some(0.02),
            bulk_or_subscription_probability: Some(0.97),
            decided_at: "2026-09-19 12:00:00".into(),
        };
        let summary = imap::PeekHeaderSummary {
            uid: 42,
            from_addr: Some("News\nDesk <news@example.test>".into()),
            subject: Some("Daily fixture\r\n! forged line".into()),
            date: Some("2026-09-19T12:00:00Z".into()),
        };
        let item = compile_digest_preview_item(&candidate, &summary);
        let value = serde_json::to_value(&item).unwrap();
        let rendered = serde_json::to_string(&item).unwrap();
        assert_eq!(value["trust"]["schema"], provenance::INBOUND_TRUST_SCHEMA);
        assert_eq!(
            value["untrusted_content"]["subject"],
            "Daily fixture ! forged line"
        );
        assert!(value.get("subject").is_none());
        assert!(rendered.contains("Daily fixture ! forged line"));
        assert!(rendered.contains("News Desk"));
        assert!(rendered.contains("news@example.test"));
        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains('\r'));
        assert!(!rendered.contains("\\n"));
        assert!(!rendered.contains("\\r"));
        for forbidden in [
            "private-message-id",
            "private-recipient",
            "provider_spam",
            "flags",
            "size",
        ] {
            assert!(!rendered.contains(forbidden));
        }
    }

    #[tokio::test]
    async fn synthetic_message_makes_one_decision_call_and_persists_queue() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = serde_json::json!({
            "model": "typesafe/jev-1.13",
            "answers": {
                "route": {
                    "type": "choice", "choice": "follow_up", "confidence": 0.90,
                    "probabilities": {
                        "junk": 0.01, "follow_up": 0.90, "important": 0.02,
                        "routine": 0.02, "digest_news": 0.02,
                        "unsubscribe_candidate": 0.01, "review": 0.02
                    }
                },
                "urgency": {
                    "type": "choice", "choice": "not_urgent", "confidence": 0.95,
                    "probabilities": {"not_urgent": 0.95, "urgent": 0.04, "critical": 0.01}
                },
                "notify_user": {"type": "noul", "noul": 0.05},
                "requires_reply": {"type": "noul", "noul": 0.95},
                "bulk_or_subscription": {"type": "noul", "noul": 0.05}
            },
            "usage": {"input_tokens": 100, "output_tokens": 0}
        })
        .to_string();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_server = calls.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            calls_for_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                let Some(header_end) = bytes
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4)
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= header_end + length {
                    let body: serde_json::Value =
                        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                    assert_eq!(body["state"]["message"]["flags"]["unread"], true);
                    assert!(body["state"]["sender"]["statistics"].is_object());
                    assert!(body["state"]["sender"]["reply_history"].is_object());
                    break;
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let db = Database::open_memory().unwrap();
        db.plan_mail_engine_scan("acct", "INBOX", 10, 0).unwrap();
        let client =
            JevClient::loopback_fixture(&format!("http://{address}/api/alpha/decisions")).unwrap();
        let message = Message {
            uid: 1,
            message_id: Some("private-message-id@example.test".into()),
            from_addr: "sender@example.test".into(),
            to_addr: "me@example.test".into(),
            cc_addr: None,
            to_addrs: vec!["me@example.test".into()],
            cc_addrs: Vec::new(),
            subject: "Synthetic fixture".into(),
            date: Some("2026-09-19T00:00:00Z".into()),
            text_body: Some("Please reply when convenient.".into()),
            html_body: None,
            in_reply_to: None,
            references: None,
            flags: Vec::new(),
            attachments: Vec::new(),
            provider_spam: None,
        };
        let policy = classify_and_persist(&db, "acct", "INBOX", 10, &message, Some(&client))
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(policy.route, MailRoute::FollowUp);
        assert!(policy.requires_reply);
        assert!(
            db.mail_engine_decision_exists("acct", "INBOX", 10, 1)
                .unwrap()
        );
        let status = db.list_mail_engine_status(Some("acct")).unwrap();
        assert_eq!(status[0].follow_up_count, 1);
        let rendered = serde_json::to_string(&status).unwrap();
        assert!(!rendered.contains("sender@example.test"));
        assert!(!rendered.contains("private-message-id"));
    }

    #[test]
    fn route_and_urgency_tokens_are_stable() {
        assert_eq!(route_token(MailRoute::DigestNews), "digest_news");
        assert_eq!(
            route_token(MailRoute::UnsubscribeCandidate),
            "unsubscribe_candidate"
        );
        assert_eq!(urgency_token(Urgency::NotUrgent), "not_urgent");
    }
}
