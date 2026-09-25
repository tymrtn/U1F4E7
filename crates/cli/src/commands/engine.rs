// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use envelope_email_store::credential_store::{self, CredentialBackend};
use envelope_email_store::models::{Account, Event, Message};
use envelope_email_store::{
    Database, MAIL_ENGINE_SCHEMA_VERSION, MailEngineDecisionClaim, MailEngineDecisionRecovery,
    MailEngineDigestCandidate, MailEngineDigestKey, MailEngineSenderStats, MailboxScanPlan,
    NewMailEngineDecision, mail_engine_hash,
};
use envelope_email_transport::decisions::DecisionsConfig;
use envelope_email_transport::event_delivery::{DeliveryLimits, deliver_due_events};
use envelope_email_transport::folders;
use envelope_email_transport::http::Allowance;
use envelope_email_transport::imap;
use envelope_email_transport::jev::{
    self, DecisionsProvider, JevBackend, JevClient, JevClientError, JevState, MailRoute,
    MessageFlags, PastInteractions, PolicyDecision, ReplyHistory, SenderState, SenderStatistics,
    Urgency, ValidatedDecision, apply_policy, build_request,
};
use envelope_email_transport::rule_exec::{
    self, ActionAttribution, ActionSource, ExecStatus, MessageTarget,
};
use envelope_email_transport::rules::MessageContext;
use serde::Serialize;

use super::{common::resolve_account, provenance};

const MAX_MESSAGES_PER_PASS: usize = 100;

#[derive(Debug, Clone)]
pub struct EngineOptions<'a> {
    pub account: Option<&'a str>,
    pub folder: &'a str,
    pub apply: bool,
    pub deliver: bool,
    pub json: bool,
    pub backend: CredentialBackend,
    /// `--jev-backend`: overrides `decisions.provider` for this run.
    pub jev_backend: Option<JevBackend>,
}

#[derive(Debug, Serialize)]
struct EnginePassReport {
    mode: &'static str,
    interval_seconds: Option<u64>,
    apply: bool,
    backend: &'static str,
    model: String,
    interrupted_decisions_recovered: usize,
    accounts: Vec<AccountReport>,
    delivery: Option<DeliveryPassReport>,
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
    notifications_enqueued: usize,
    /// Confirm offers minted from confident routes (`decisions.propose_actions`).
    actions_offered: usize,
    error_code: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeliveryPassReport {
    examined: usize,
    delivered: usize,
    retried: usize,
    dead_lettered: usize,
    skipped: usize,
}

#[derive(Debug, Serialize)]
struct RecoveryReport {
    account_id: String,
    folder: String,
    uid: u32,
    outcome: &'static str,
    new_jev_call_authorized: bool,
    backend: Option<&'static str>,
    model: Option<String>,
    route: Option<String>,
}

#[derive(Debug, Serialize)]
struct DigestPreviewReport {
    items: Vec<DigestPreviewItem>,
    errors: Vec<DigestPreviewError>,
    consumed_count: usize,
    remaining_count: usize,
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
            notifications_enqueued: 0,
            actions_offered: 0,
            error_code: None,
        }
    }
}

#[tokio::main]
pub async fn run_once(options: EngineOptions<'_>) -> Result<()> {
    let decisions = DecisionsConfig::load()?;
    let provider = decisions.provider_for(options.jev_backend)?;
    let (accounts, interrupted_decisions_recovered) =
        process_once(&options, &decisions, &provider).await?;
    let delivery = drain_due_event_deliveries(options.deliver).await?;
    print_report(
        &EnginePassReport {
            mode: "once",
            interval_seconds: None,
            apply: options.apply,
            backend: provider.backend.as_str(),
            model: provider.model.clone(),
            interrupted_decisions_recovered,
            accounts,
            delivery,
        },
        options.json,
    )
}

#[tokio::main]
pub async fn run_loop(options: EngineOptions<'_>, interval_seconds: u64) -> Result<()> {
    if interval_seconds < 60 {
        bail!("engine interval must be at least 60 seconds");
    }
    let decisions = DecisionsConfig::load()?;
    let provider = decisions.provider_for(options.jev_backend)?;
    loop {
        let (accounts, interrupted_decisions_recovered) =
            process_once(&options, &decisions, &provider).await?;
        let delivery = drain_due_event_deliveries(options.deliver).await?;
        print_report(
            &EnginePassReport {
                mode: "run",
                interval_seconds: Some(interval_seconds),
                apply: options.apply,
                backend: provider.backend.as_str(),
                model: provider.model.clone(),
                interrupted_decisions_recovered,
                accounts,
                delivery,
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

async fn drain_due_event_deliveries(deliver: bool) -> Result<Option<DeliveryPassReport>> {
    if !deliver {
        return Ok(None);
    }
    let db = Database::open_default().context("failed to open database for event delivery")?;
    let report = deliver_due_events(
        &db,
        &Allowance::Public,
        chrono::Utc::now(),
        DeliveryLimits::default(),
    )
    .await
    .context("event delivery executor failed")?;
    Ok(Some(DeliveryPassReport {
        examined: report.examined,
        delivered: report.delivered,
        retried: report.retried,
        dead_lettered: report.dead_lettered,
        skipped: report.skipped,
    }))
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
                "{} {}: {} decisions — {} follow-up, {} important, {} urgent, {} digest, {} junk, {} routine, {} review, {} processing, {} unsubscribe candidates",
                item.account_id,
                item.folder,
                item.decision_count,
                item.follow_up_count,
                item.important_count,
                item.notification_count,
                item.digest_news_count,
                item.junk_count,
                item.routine_count,
                item.review_count,
                item.processing_count,
                item.unsubscribe_candidate_count,
            );
            println!(
                "  New-mail watermark: UID {} (UIDVALIDITY {})",
                item.last_seen_uid, item.uidvalidity
            );
            if let Some(error_code) = item.last_error.as_deref() {
                print_engine_problem(error_code, "  ");
            }
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct LayaHealthReport {
    backend: &'static str,
    endpoint: &'static str,
    expected_model: &'static str,
    expected_revision: &'static str,
    durable_model_identity: &'static str,
    content_egress: bool,
    fallback_to_openrouter: bool,
    ready: bool,
    status: Option<String>,
    dtype: Option<String>,
    error_code: Option<&'static str>,
}

/// Inspect the local Laya provider's readiness and model identity. It sends no
/// message, sender, or history content.
#[tokio::main]
pub async fn run_laya_health(json: bool) -> Result<()> {
    let health = jev::laya_health().await;
    let report = match &health {
        Ok(health) => LayaHealthReport {
            backend: JevBackend::Laya.as_str(),
            endpoint: jev::LAYA_HEALTH_ENDPOINT,
            expected_model: jev::LAYA_MODEL_REPO,
            expected_revision: jev::LAYA_MODEL_REVISION,
            durable_model_identity: jev::LAYA_JEV_MODEL,
            content_egress: false,
            fallback_to_openrouter: false,
            ready: health.ready,
            status: Some(health.status.clone()),
            dtype: Some(health.dtype.clone()),
            error_code: None,
        },
        Err(error) => LayaHealthReport {
            backend: JevBackend::Laya.as_str(),
            endpoint: jev::LAYA_HEALTH_ENDPOINT,
            expected_model: jev::LAYA_MODEL_REPO,
            expected_revision: jev::LAYA_MODEL_REVISION,
            durable_model_identity: jev::LAYA_JEV_MODEL,
            content_egress: false,
            fallback_to_openrouter: false,
            ready: false,
            status: None,
            dtype: None,
            error_code: Some(laya_health_error_code(error)),
        },
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.ready {
        println!(
            "Local Laya provider is ready at {} serving {}@{} ({}).",
            report.endpoint,
            report.expected_model,
            report.expected_revision,
            report.dtype.as_deref().unwrap_or("unknown dtype"),
        );
        println!(
            "  Durable model identity: {}",
            report.durable_model_identity
        );
        println!("  No content egress. No fallback to OpenRouter.");
    } else {
        println!("Local Laya provider is not usable at {}.", report.endpoint);
        print_engine_problem(report.error_code.unwrap_or("laya_jev_failed"), "  ");
    }
    if report.ready {
        Ok(())
    } else {
        bail!("the local Laya provider is not serving the pinned checkpoint")
    }
}

fn laya_health_error_code(error: &jev::JevClientError) -> &'static str {
    match error {
        jev::JevClientError::LayaModelMismatch => "laya_model_mismatch",
        _ => "laya_provider_unavailable",
    }
}

pub fn run_decisions(
    account: Option<&str>,
    route: Option<&str>,
    status: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let account_id = account
        .map(|value| resolve_account(&db, Some(value)).map(|account| account.id))
        .transpose()?;
    let decisions = db.list_mail_engine_decisions(account_id.as_deref(), route, status, limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&decisions)?);
    } else if decisions.is_empty() {
        println!("No current mail-engine decisions match those filters.");
    } else {
        for item in decisions {
            let route_label = human_route_label(&item.effective_route);
            let confidence = item
                .route_confidence
                .map(|value| format!("{:.0}% confidence", value * 100.0))
                .unwrap_or_else(|| "confidence unavailable".into());
            println!(
                "{} · {} {} UID {} · {} · {}",
                route_label, item.account_id, item.folder, item.uid, confidence, item.decided_at,
            );
            println!(
                "  State: {} · urgency: {} · action: {}",
                item.status,
                item.effective_urgency,
                item.executed_action
                    .as_deref()
                    .unwrap_or(item.execution_status.as_str()),
            );
            println!("  Model: {}/{}", item.backend, item.model);
            if item.correction_revision > 0 {
                println!(
                    "  Human correction r{}: model route {} / urgency {}",
                    item.correction_revision, item.route, item.urgency
                );
            }
            if let Some(error_code) = item.error_code.as_deref() {
                print_engine_problem(error_code, "  ");
            }
        }
    }
    Ok(())
}

pub fn run_correct(
    uid: u32,
    account: &str,
    folder: &str,
    route: &str,
    urgency: &str,
    expected_revision: u64,
    json: bool,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let account_id = resolve_account(&db, Some(account))?.id;
    let revision = db
        .correct_current_mail_engine_decision(
            &account_id,
            folder,
            uid,
            expected_revision,
            route,
            urgency,
            "cli",
        )?
        .context(
            "decision was not found, is busy, or has a newer correction; inspect `engine decisions` and retry with its current revision",
        )?;
    let decision = db
        .list_mail_engine_decisions(Some(&account_id), None, None, 200)?
        .into_iter()
        .find(|item| item.folder == folder && item.uid == uid)
        .context("corrected decision could not be read back")?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "revision": revision,
                "decision": decision,
                "mailbox_action_changed": decision.executed_action.as_deref() == Some("cancelled_by_human_correction"),
            }))?
        );
    } else {
        println!(
            "Corrected UID {} to {} / {} at revision {}. The original Jev result remains in the audit record.",
            uid,
            human_route_label(&decision.effective_route),
            decision.effective_urgency,
            revision
        );
        if decision.execution_status == "completed" {
            println!(
                "Mailbox action already completed; classification changed, but mailbox restoration is manual."
            );
        } else if decision.executed_action.as_deref() == Some("cancelled_by_human_correction") {
            println!("The pending junk action was cancelled before it could move the message.");
        }
    }
    Ok(())
}

#[tokio::main]
pub async fn run_recover(
    uid: u32,
    account: &str,
    folder: &str,
    retry_jev: bool,
    confirm_new_jev_call: bool,
    json: bool,
    backend: CredentialBackend,
    jev_backend: Option<JevBackend>,
) -> Result<()> {
    let provider = DecisionsConfig::load()?.provider_for(jev_backend)?;
    let provider = &provider;
    if retry_jev && !confirm_new_jev_call {
        bail!("--retry-jev requires --confirm-new-jev-call because it authorizes a new model call");
    }
    let db = Database::open_default().context("failed to open database")?;
    let account_record = resolve_account(&db, Some(account))?;
    let account_id = account_record.id.clone();
    let route = if retry_jev {
        if provider.api_key().is_err() {
            bail!(
                "{} is still missing; no retry was attempted",
                provider.key_env.as_deref().unwrap_or("the API key")
            );
        }
        let retryable_error = retryable_error_code(provider.backend);
        let candidate = db
            .list_mail_engine_decisions(Some(&account_id), Some("review"), Some("review"), 200)?
            .into_iter()
            .find(|item| item.folder == folder && item.uid == uid)
            .filter(|item| {
                item.backend == provider.backend.as_str()
                    && item.model == provider.model
                    && item.error_code.as_deref() == Some(retryable_error)
            })
            .context("this decision is not retryable with the selected Jev backend and model")?;
        let passphrase = credential_store::get_passphrase(backend)
            .context("credential store is not available for safe retry")?;
        let credentials = db
            .get_account_with_credentials(&account_id, &passphrase)
            .context("failed to decrypt account credentials for safe retry")?;
        let mut client = imap::connect(&credentials)
            .await
            .context("failed to connect to IMAP for safe retry")?;
        let selected = imap::examine_folder_info(&mut client, folder)
            .await
            .context("failed to examine mailbox for safe retry")?;
        if selected.uid_validity != Some(candidate.uidvalidity) {
            bail!("mailbox UIDVALIDITY changed; the historical decision was not retried");
        }
        let mut messages =
            imap::fetch_raw_messages_selected_uid_set(&mut client, folder, &uid.to_string())
                .await
                .context("failed to fetch the exact message for safe retry")?;
        if messages.len() != 1 {
            bail!("the exact message is no longer available; no retry was attempted");
        }
        let message = imap::parse_raw_message(&messages.remove(0))
            .context("failed to parse the exact message for safe retry")?;
        let prepared_uidvalidity = db
            .prepare_current_mail_engine_safe_retry(
                &account_id,
                folder,
                uid,
                provider.backend.as_str(),
                &provider.model,
                retryable_error,
            )?
            .context("the decision changed before retry; no new Jev call was made")?;
        if prepared_uidvalidity != candidate.uidvalidity {
            bail!("the decision epoch changed before retry; no new Jev call was made");
        }
        let policy = classify_and_persist(
            &db,
            &account_id,
            folder,
            candidate.uidvalidity,
            &message,
            None,
            true,
            provider,
        )
        .await?
        .context("another worker claimed the decision; no duplicate Jev call was made")?;
        Some(route_token(policy.route).to_string())
    } else {
        let changed =
            db.resolve_current_mail_engine_processing_as_review(&account_id, folder, uid)?;
        if !changed {
            bail!(
                "no abandoned current-epoch processing decision for account {account_id}, folder {folder}, UID {uid}"
            );
        }
        None
    };
    let report = RecoveryReport {
        account_id,
        folder: folder.to_string(),
        uid,
        outcome: if retry_jev {
            "fresh_jev_decision_persisted"
        } else {
            "released_to_human_review"
        },
        new_jev_call_authorized: retry_jev,
        backend: retry_jev.then_some(provider.backend.as_str()),
        model: retry_jev.then(|| provider.model.clone()),
        route,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if retry_jev {
        println!(
            "UID {} was reconsidered with one explicitly authorized Jev call. Route: {}.",
            uid,
            report.route.as_deref().unwrap_or("review")
        );
    } else {
        println!(
            "UID {} was released from processing into human review. No new Jev call was made.",
            uid
        );
    }
    Ok(())
}

fn human_route_label(route: &str) -> &'static str {
    match route {
        "junk" => "Junk",
        "follow_up" => "Needs reply",
        "important" => "Important",
        "routine" => "Routine",
        "digest_news" => "News digest",
        "unsubscribe_candidate" => "Unsubscribe candidate",
        "review" => "Needs review",
        _ => "Unknown decision",
    }
}

fn print_engine_problem(error_code: &str, indent: &str) {
    println!("{indent}Problem: {error_code}");
    println!(
        "{indent}Next step: {}",
        engine_error_remediation(error_code)
    );
}

fn engine_error_remediation(error_code: &str) -> &'static str {
    match error_code {
        "jev_request_failed" => {
            "check OPENROUTER_API_KEY and connectivity, then inspect this decision before retrying"
        }
        "laya_jev_failed" => {
            "run `envelope engine laya-health` to confirm the local Laya provider is serving the pinned checkpoint, then inspect or explicitly retry this decision with --jev-backend laya"
        }
        "decision_incomplete" => {
            "another pass may still be working; stale claims are moved to review after ten minutes"
        }
        "decision_interrupted" => {
            "the prior worker stopped before a durable result; inspect and classify this message manually"
        }
        "notification_enqueue_failed" => {
            "check event routes and delivery storage; the watermark remains held for a safe retry"
        }
        "laya_provider_unavailable" => {
            "start the local Laya provider: python3 scripts/laya_jev_provider.py serve"
        }
        "laya_model_mismatch" => {
            "the local provider is serving other weights; restart it on the pinned aac6fef/laya-mlx revision"
        }
        "credential_decrypt_failed" => "repair the Envelope credential store for this account",
        "imap_connect_failed" => "check account credentials and IMAP connectivity",
        "imap_examine_failed" => "check that the configured folder still exists and is readable",
        "uidvalidity_changed" => {
            "Envelope safely rebaselined; only newer messages will be processed"
        }
        "highest_uid_unavailable" => "retry after the IMAP server returns a stable UID boundary",
        "message_fetch_failed" | "message_parse_failed" => {
            "open the message directly and review it manually"
        }
        "spam_folder_not_found" => "configure or create the account's Junk/Spam folder",
        "imap_move_failed" => "the message was not moved; retry after checking IMAP capabilities",
        "operator_retry_requested" => {
            "run `envelope engine once` or keep `engine run` active to issue the authorized fresh decision"
        }
        "processing_recovered_for_review" => {
            "inspect the message and either leave it for human review or explicitly retry Jev"
        }
        "openrouter_api_key_missing" => {
            "export OPENROUTER_API_KEY, then use `engine recover --retry-jev --confirm-new-jev-call`"
        }
        _ => "inspect `envelope engine decisions --status review` for the affected message",
    }
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
    consume: bool,
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
                consumed_count: 0,
                remaining_count: 0,
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
        consumed_count: 0,
        remaining_count: 0,
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
    if consume && !report.items.is_empty() {
        let keys = report
            .items
            .iter()
            .map(|item| MailEngineDigestKey {
                account_id: item.account_id.clone(),
                folder: item.folder.clone(),
                uidvalidity: item.uidvalidity,
                uid: item.uid,
            })
            .collect::<Vec<_>>();
        report.consumed_count = db.consume_mail_engine_digest(&keys)?;
    }
    report.remaining_count = db.count_pending_mail_engine_digest(account_id.as_deref())?;
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
        if report.consumed_count > 0 {
            println!(
                "Consumed {} compiled item(s); {} remain queued. Mailbox read state was not changed.",
                report.consumed_count, report.remaining_count
            );
        } else {
            println!("{} item(s) remain queued.", report.remaining_count);
        }
    }
    Ok(())
}

fn display_probability(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.3}"))
        .unwrap_or_else(|| "unknown".into())
}

async fn process_once(
    options: &EngineOptions<'_>,
    decisions: &DecisionsConfig,
    provider: &DecisionsProvider,
) -> Result<(Vec<AccountReport>, usize)> {
    let db = Database::open_default().context("failed to open database")?;
    let interrupted_decisions_recovered = db.recover_stale_mail_engine_processing(600)?;
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
                    if let Some(existing) = db.get_mail_engine_decision_execution(
                        &account.id,
                        options.folder,
                        uidvalidity,
                        uid,
                    )? {
                        if !recovery_is_terminal(&existing) {
                            report.status = "held".into();
                            report.error_code = Some("decision_incomplete".into());
                            break;
                        }
                        if let Some(urgency) = recovery_notification_urgency(&existing) {
                            match persist_urgent_event(
                                &db,
                                &account.id,
                                options.folder,
                                uidvalidity,
                                uid,
                                urgency,
                            ) {
                                Ok(enqueued) => report.notifications_enqueued += enqueued,
                                Err(_) => {
                                    report.actions_blocked += 1;
                                    report.error_code = Some("notification_enqueue_failed".into());
                                    break;
                                }
                            }
                        }
                        if options.apply
                            && existing.route == "junk"
                            && existing.execution_status == "pending"
                        {
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
                                provider,
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
                                provider,
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
                    let Some(policy) = classify_and_persist(
                        &db,
                        &account.id,
                        options.folder,
                        uidvalidity,
                        &message,
                        None,
                        false,
                        provider,
                    )
                    .await?
                    else {
                        // Another worker won the durable claim after the
                        // initial lookup. Its row may still be `processing`;
                        // stop this ordered pass rather than leap the
                        // watermark over unfinished work.
                        report.status = "held".into();
                        report.error_code = Some("decision_incomplete".into());
                        break;
                    };
                    if policy.route == MailRoute::Review {
                        report.review += 1;
                    } else {
                        report.decided += 1;
                    }
                    if policy.notify_user_now {
                        match persist_urgent_event(
                            &db,
                            &account.id,
                            options.folder,
                            uidvalidity,
                            uid,
                            policy.urgency,
                        ) {
                            Ok(enqueued) => report.notifications_enqueued += enqueued,
                            Err(_) => {
                                report.actions_blocked += 1;
                                report.error_code = Some("notification_enqueue_failed".into());
                                break;
                            }
                        }
                    }
                    match propose_route_action(
                        &db,
                        decisions,
                        &account.id,
                        &account.username,
                        options.folder,
                        uidvalidity,
                        &message,
                        &policy,
                    )
                    .await
                    {
                        Ok(true) => report.actions_offered += 1,
                        Ok(false) => {}
                        Err(_) => {
                            report.actions_blocked += 1;
                            report.error_code = Some("action_offer_failed".into());
                        }
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
    Ok((reports, interrupted_decisions_recovered))
}

/// Mint a Confirm offer from a confident route (D13). Never executes it:
/// the offer waits in `envelope actions pending` for a human.
async fn propose_route_action(
    db: &Database,
    decisions: &DecisionsConfig,
    account_id: &str,
    account_email: &str,
    folder: &str,
    uidvalidity: u32,
    message: &Message,
    policy: &PolicyDecision,
) -> Result<bool> {
    let Some((prompt, then)) = decisions.offer_for(policy) else {
        return Ok(false);
    };
    let ctx = MessageContext {
        from_addr: message.from_addr.clone(),
        to_addr: message.to_addr.clone(),
        subject: message.subject.clone(),
        tags: Vec::new(),
        scores: HashMap::new(),
        contact_tags: Vec::new(),
    };
    let message_id = message
        .message_id
        .as_deref()
        .map(envelope_email_store::canonical_message_id)
        .filter(|id| !id.is_empty());
    let target = MessageTarget {
        account_id,
        account_email,
        folder,
        uid: message.uid,
        message_id,
        ctx: &ctx,
    };
    let attribution = ActionAttribution {
        source: ActionSource::Jev,
        agent_id: None,
        event_id: Some(format!(
            "jev:{account_id}:{folder}:{uidvalidity}:{}",
            message.uid
        )),
    };
    let outcome = rule_exec::propose(db, &target, &prompt, &then, &attribution).await?;
    Ok(outcome.status == ExecStatus::Offered)
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
    claim_already_owned: bool,
    provider: &DecisionsProvider,
) -> Result<Option<PolicyDecision>> {
    let sender_address = message.from_addr.trim();
    if sender_address.is_empty() {
        persist_review(
            db,
            account_id,
            folder,
            uidvalidity,
            message.uid,
            provider,
            "sender_missing",
        )?;
        return Ok(Some(review_policy()));
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
    let request = build_request(state, &provider.model);
    let input_hash = mail_engine_hash(&serde_json::to_string(&request)?);
    let claim = MailEngineDecisionClaim {
        account_id,
        folder,
        uidvalidity,
        uid: message.uid,
        input_hash: &input_hash,
        backend: provider.backend.as_str(),
        model: &provider.model,
    };
    if claim_already_owned {
        if !db.mail_engine_processing_claim_matches(&claim)? {
            bail!("safe retry claim no longer matches the immutable Jev input");
        }
    } else if !db.claim_mail_engine_decision(&claim)? {
        // Another worker already owns or completed this immutable input. Never
        // issue a second paid request and never let this caller advance the
        // ordered watermark until a later pass observes a terminal row.
        return Ok(None);
    }
    let owned_client = if client_override.is_none() {
        // No fallback: a failure fails closed to review below and never
        // reaches another provider.
        match JevClient::for_provider(provider).await {
            Ok(client) => Some(client),
            Err(error) => {
                let code = match error {
                    JevClientError::MissingApiKey(_) => "openrouter_api_key_missing",
                    _ => backend_failure_code(provider.backend),
                };
                persist_review_with_hash(
                    db,
                    account_id,
                    folder,
                    uidvalidity,
                    message.uid,
                    &input_hash,
                    provider,
                    code,
                )?;
                return Ok(Some(review_policy()));
            }
        }
    } else {
        None
    };
    let client = client_override
        .or(owned_client.as_ref())
        .expect("Jev client exists");
    if client.backend() != provider.backend {
        bail!("Jev test client backend does not match the durable claim identity");
    }
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
                provider,
                backend_failure_code(provider.backend),
            )?;
            return Ok(Some(review_policy()));
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
        provider,
    )?;
    Ok(Some(policy))
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
    provider: &DecisionsProvider,
) -> Result<()> {
    let decision_json = serde_json::to_string(decision)?;
    let finalized = db.finalize_mail_engine_decision(&NewMailEngineDecision {
        account_id,
        folder,
        uidvalidity,
        uid,
        input_hash,
        backend: provider.backend.as_str(),
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
    provider: &DecisionsProvider,
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
        provider,
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
    provider: &DecisionsProvider,
    error_code: &str,
) -> Result<()> {
    let decision_json = serde_json::json!({"error_code": error_code}).to_string();
    let decision = NewMailEngineDecision {
        account_id,
        folder,
        uidvalidity,
        uid,
        input_hash,
        backend: provider.backend.as_str(),
        model: &provider.model,
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

fn recovery_is_terminal(existing: &MailEngineDecisionRecovery) -> bool {
    matches!(existing.status.as_str(), "decided" | "review")
}

fn recovery_notification_urgency(existing: &MailEngineDecisionRecovery) -> Option<Urgency> {
    if existing.notify_user_probability.unwrap_or(0.0) < 0.90 {
        return None;
    }
    match existing.urgency.as_str() {
        "urgent" => Some(Urgency::Urgent),
        "critical" => Some(Urgency::Critical),
        _ => None,
    }
}

fn persist_urgent_event(
    db: &Database,
    account_id: &str,
    folder: &str,
    uidvalidity: u32,
    uid: u32,
    urgency: Urgency,
) -> Result<usize> {
    let marker = mail_engine_hash(&format!(
        "{account_id}:{folder}:{uidvalidity}:{uid}:mail_engine_urgent"
    ));
    let event = Event {
        id: marker.clone(),
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
    db.insert_event_idempotent(&event)?;
    Ok(super::watch::enqueue_deliveries_for_event(db, &event)?)
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

fn backend_failure_code(backend: JevBackend) -> &'static str {
    match backend {
        JevBackend::Laya => "laya_jev_failed",
        JevBackend::Openrouter | JevBackend::Custom => "jev_request_failed",
    }
}

fn retryable_error_code(backend: JevBackend) -> &'static str {
    match backend {
        JevBackend::Laya => "laya_jev_failed",
        JevBackend::Openrouter | JevBackend::Custom => "openrouter_api_key_missing",
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
        if report.interrupted_decisions_recovered > 0 {
            println!(
                "Recovered {} interrupted decision(s) into human review without another Jev call.",
                report.interrupted_decisions_recovered
            );
        }
        for account in &report.accounts {
            println!(
                "{}: {} — {} examined, {} decided, {} review, {} actions completed, {} blocked, {} urgent deliveries enqueued",
                account.account_id,
                account.status,
                account.examined,
                account.decided,
                account.review,
                account.actions_completed,
                account.actions_blocked,
                account.notifications_enqueued,
            );
            if let Some(error_code) = account.error_code.as_deref() {
                print_engine_problem(error_code, "  ");
            }
        }
        if let Some(delivery) = &report.delivery {
            println!(
                "event delivery: {} examined, {} delivered, {} retried, {} dead-lettered, {} skipped",
                delivery.examined,
                delivery.delivered,
                delivery.retried,
                delivery.dead_lettered,
                delivery.skipped,
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
    fn persisted_notification_state_recovers_only_high_confidence_urgent_routes() {
        let recovery = |urgency: &str, probability| MailEngineDecisionRecovery {
            status: "decided".into(),
            route: "important".into(),
            execution_status: "not_requested".into(),
            urgency: urgency.into(),
            notify_user_probability: probability,
        };
        let mut processing = recovery("not_urgent", None);
        processing.status = "processing".into();
        assert!(!recovery_is_terminal(&processing));
        assert!(recovery_is_terminal(&recovery("not_urgent", None)));
        assert_eq!(
            recovery_notification_urgency(&recovery("critical", Some(0.95))),
            Some(Urgency::Critical)
        );
        assert_eq!(
            recovery_notification_urgency(&recovery("urgent", Some(0.90))),
            Some(Urgency::Urgent)
        );
        assert_eq!(
            recovery_notification_urgency(&recovery("urgent", Some(0.89))),
            None
        );
        assert_eq!(
            recovery_notification_urgency(&recovery("not_urgent", Some(0.99))),
            None
        );
    }

    #[test]
    fn urgent_event_recovery_enqueues_one_idempotent_delivery_without_message_content() {
        let db = Database::open_memory().unwrap();
        db.create_event_route(
            "acct",
            r#"{"event_types":["mail_engine_urgent"]}"#,
            r#"{"type":"webhook","url":"https://example.test/urgent"}"#,
            true,
            100,
        )
        .unwrap();

        assert_eq!(
            persist_urgent_event(&db, "acct", "INBOX", 10, 42, Urgency::Critical).unwrap(),
            1
        );
        assert_eq!(
            persist_urgent_event(&db, "acct", "INBOX", 10, 42, Urgency::Critical).unwrap(),
            0
        );

        let marker = mail_engine_hash("acct:INBOX:10:42:mail_engine_urgent");
        let event = db.get_event(&marker).unwrap().unwrap();
        assert_eq!(event.id, marker);
        assert_eq!(event.event_type, "mail_engine_urgent");
        assert!(event.message_id.is_none());
        assert!(event.from_addr.is_none());
        assert!(event.subject.is_none());
        assert!(event.snippet.is_none());
        let payload: serde_json::Value =
            serde_json::from_str(event.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["urgency"], "critical");
        assert_eq!(payload["source"], "jev");

        let due = db
            .list_due_deliveries(
                &(chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
                10,
            )
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].event_id, marker);
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
    async fn contended_decision_claim_returns_hold_without_calling_jev() {
        let db = Database::open_memory().unwrap();
        db.plan_mail_engine_scan("acct", "INBOX", 10, 0).unwrap();
        assert!(
            db.claim_mail_engine_decision(&MailEngineDecisionClaim {
                account_id: "acct",
                folder: "INBOX",
                uidvalidity: 10,
                uid: 1,
                input_hash: "claimed-by-another-worker",
                backend: "openrouter",
                model: "typesafe/jev-1.13",
            })
            .unwrap()
        );
        // Port 9 has no fixture server. A network attempt would fail the test;
        // claim contention must return before any paid/model request.
        let client = JevClient::loopback_fixture("http://127.0.0.1:9/api/alpha/decisions")
            .await
            .unwrap();
        let message = Message {
            uid: 1,
            message_id: Some("contended@example.test".into()),
            from_addr: "sender@example.test".into(),
            to_addr: "me@example.test".into(),
            cc_addr: None,
            to_addrs: vec!["me@example.test".into()],
            cc_addrs: Vec::new(),
            subject: "Contended fixture".into(),
            date: Some("2026-09-19T00:00:00Z".into()),
            text_body: Some("This must not reach JEV.".into()),
            html_body: None,
            in_reply_to: None,
            references: None,
            flags: Vec::new(),
            attachments: Vec::new(),
            provider_spam: None,
        };

        let outcome = classify_and_persist(
            &db,
            "acct",
            "INBOX",
            10,
            &message,
            Some(&client),
            false,
            &DecisionsProvider::openrouter(),
        )
        .await
        .unwrap();

        assert!(outcome.is_none());
        let recovery = db
            .get_mail_engine_decision_execution("acct", "INBOX", 10, 1)
            .unwrap()
            .unwrap();
        assert_eq!(recovery.status, "processing");
        assert!(!recovery_is_terminal(&recovery));
        assert!(matches!(
            db.plan_mail_engine_scan("acct", "INBOX", 10, 2).unwrap(),
            MailboxScanPlan::NewRange { after_uid: 0, .. }
        ));
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
        let response_for_server = response.clone();
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
                response_for_server.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket
                .write_all(response_for_server.as_bytes())
                .await
                .unwrap();
        });

        let db = Database::open_memory().unwrap();
        db.plan_mail_engine_scan("acct", "INBOX", 10, 0).unwrap();
        let client = JevClient::loopback_fixture(&format!("http://{address}/api/alpha/decisions"))
            .await
            .unwrap();
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
        let policy = classify_and_persist(
            &db,
            "acct",
            "INBOX",
            10,
            &message,
            Some(&client),
            false,
            &DecisionsProvider::openrouter(),
        )
        .await
        .unwrap()
        .expect("the uncontended fixture should return a policy");
        server.await.unwrap();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(policy.route, MailRoute::FollowUp);
        assert!(policy.requires_reply);
        assert!(
            db.mail_engine_decision_exists("acct", "INBOX", 10, 1)
                .unwrap()
        );

        db.conn()
            .execute(
                "UPDATE mail_engine_decisions
                 SET status = 'review', route = 'review', urgency = 'not_urgent',
                     route_probability = NULL, route_confidence = NULL,
                     notify_user_probability = NULL,
                     requires_reply_probability = NULL,
                     bulk_or_subscription_probability = NULL,
                     decision_json = '{\"error_code\":\"openrouter_api_key_missing\"}',
                     execution_status = 'not_requested', executed_action = NULL,
                     last_error = NULL
                 WHERE account_id = 'acct' AND folder = 'INBOX'
                   AND uidvalidity = 10 AND uid = 1",
                [],
            )
            .unwrap();
        assert_eq!(
            db.prepare_current_mail_engine_safe_retry(
                "acct",
                "INBOX",
                1,
                "openrouter",
                "typesafe/jev-1.13",
                "openrouter_api_key_missing",
            )
            .unwrap(),
            Some(10)
        );
        let retry_input_hash = db
            .conn()
            .query_row(
                "SELECT input_hash FROM mail_engine_decisions
                 WHERE account_id = 'acct' AND folder = 'INBOX'
                   AND uidvalidity = 10 AND uid = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert!(
            db.mail_engine_processing_claim_matches(&MailEngineDecisionClaim {
                account_id: "acct",
                folder: "INBOX",
                uidvalidity: 10,
                uid: 1,
                input_hash: &retry_input_hash,
                backend: "openrouter",
                model: "typesafe/jev-1.13",
            })
            .unwrap()
        );

        let retry_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let retry_address = retry_listener.local_addr().unwrap();
        let retry_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retry_calls_for_server = retry_calls.clone();
        let retry_response = response.clone();
        let retry_server = tokio::spawn(async move {
            let (mut socket, _) = retry_listener.accept().await.unwrap();
            retry_calls_for_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
                    break;
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                retry_response.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(retry_response.as_bytes()).await.unwrap();
        });
        let retry_client =
            JevClient::loopback_fixture(&format!("http://{retry_address}/api/alpha/decisions"))
                .await
                .unwrap();
        let retried = classify_and_persist(
            &db,
            "acct",
            "INBOX",
            10,
            &message,
            Some(&retry_client),
            true,
            &DecisionsProvider::openrouter(),
        )
        .await
        .unwrap()
        .expect("the owned retry claim should finalize");
        retry_server.await.unwrap();
        assert_eq!(retry_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(retried.route, MailRoute::FollowUp);

        let status = db.list_mail_engine_status(Some("acct")).unwrap();
        assert_eq!(status[0].follow_up_count, 1);
        let rendered = serde_json::to_string(&status).unwrap();
        assert!(!rendered.contains("sender@example.test"));
        assert!(!rendered.contains("private-message-id"));
    }

    #[tokio::test]
    async fn confident_route_mints_one_offer_only_when_proposals_are_on() {
        let db = Database::open_memory().unwrap();
        let message = Message {
            uid: 5,
            message_id: Some("<trip@airline.example>".into()),
            from_addr: "desk@airline.example".into(),
            to_addr: "me@example.test".into(),
            cc_addr: None,
            to_addrs: vec!["me@example.test".into()],
            cc_addrs: Vec::new(),
            subject: "Your itinerary".into(),
            date: None,
            text_body: Some("Flight on Monday.".into()),
            html_body: None,
            in_reply_to: None,
            references: None,
            flags: Vec::new(),
            attachments: Vec::new(),
            provider_spam: None,
        };
        let policy = PolicyDecision {
            route: MailRoute::FollowUp,
            urgency: Urgency::NotUrgent,
            notify_user_now: false,
            requires_reply: true,
            bulk_or_subscription: false,
            abstained: false,
        };
        let mut decisions = DecisionsConfig::default();
        macro_rules! offer {
            ($d:expr) => {
                propose_route_action(
                    &db,
                    $d,
                    "acct",
                    "me@example.test",
                    "INBOX",
                    10,
                    &message,
                    &policy,
                )
            };
        }
        assert!(!offer!(&decisions).await.unwrap(), "off by default");

        decisions.propose_actions = true;
        assert!(offer!(&decisions).await.unwrap());
        assert!(!offer!(&decisions).await.unwrap(), "a replay mints nothing");
        let events = db.list_events(Some("acct"), 10).unwrap();
        let offers: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "action_offered")
            .collect();
        assert_eq!(offers.len(), 1);
        assert_eq!(
            offers[0].message_id.as_deref(),
            Some("trip@airline.example")
        );
        let payload: serde_json::Value =
            serde_json::from_str(offers[0].payload.as_deref().unwrap()).unwrap();
        assert_eq!(
            payload["actions"],
            serde_json::json!([{"add_tag": "follow-up"}])
        );
        assert!(
            db.get_tags("acct", "trip@airline.example")
                .unwrap()
                .is_empty()
        );
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

    #[tokio::test]
    async fn laya_failures_stay_local_and_preserve_backend_model_identity() {
        let db = Database::open_memory().unwrap();
        db.plan_mail_engine_scan("acct", "INBOX", 10, 0).unwrap();
        persist_review(
            &db,
            "acct",
            "INBOX",
            10,
            1,
            &DecisionsProvider::laya(),
            backend_failure_code(JevBackend::Laya),
        )
        .unwrap();

        let decisions = db
            .list_mail_engine_decisions(Some("acct"), Some("review"), Some("review"), 10)
            .unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].backend, "laya");
        assert_eq!(decisions[0].model, jev::LAYA_JEV_MODEL);
        assert_eq!(decisions[0].route, "review");
        assert_eq!(decisions[0].error_code.as_deref(), Some("laya_jev_failed"));

        // Each backend keeps its own distinct closed error code and its own
        // retryable precondition, so a retry can never cross providers.
        assert_eq!(backend_failure_code(JevBackend::Laya), "laya_jev_failed");
        assert_eq!(
            backend_failure_code(JevBackend::Openrouter),
            "jev_request_failed"
        );
        assert_eq!(retryable_error_code(JevBackend::Laya), "laya_jev_failed");
        assert_eq!(
            retryable_error_code(JevBackend::Openrouter),
            "openrouter_api_key_missing"
        );
        assert_ne!(
            retryable_error_code(JevBackend::Laya),
            retryable_error_code(JevBackend::Openrouter)
        );

        // A laya-backed review row is not retryable under the OpenRouter
        // identity, and vice versa.
        assert_eq!(
            db.prepare_current_mail_engine_safe_retry(
                "acct",
                "INBOX",
                1,
                JevBackend::Openrouter.as_str(),
                jev::JEV_MODEL,
                retryable_error_code(JevBackend::Openrouter),
            )
            .unwrap(),
            None
        );
        assert_eq!(
            db.prepare_current_mail_engine_safe_retry(
                "acct",
                "INBOX",
                1,
                JevBackend::Laya.as_str(),
                jev::LAYA_JEV_MODEL,
                retryable_error_code(JevBackend::Laya),
            )
            .unwrap(),
            Some(10)
        );

        let client = JevClient::for_provider(&DecisionsProvider::laya())
            .await
            .unwrap();
        assert_eq!(client.backend(), JevBackend::Laya);
    }
}
