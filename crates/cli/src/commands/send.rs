// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_store::{CredentialBackend, Database, Event};
use envelope_email_transport::SmtpSender;
use envelope_email_transport::attribution_persist::success_attribution_block;
use envelope_email_transport::outbound::{
    IMMEDIATE_SEND_CONFIRM_CODE, OUTBOX_COOLDOWN_REASON, OUTBOX_COOLDOWN_REASON_CODE,
    SendDisposition, SendSurface, resolve_cooldown_seconds, resolve_disposition,
};
use envelope_email_transport::smtp::Attachment;
use envelope_email_transport::{
    SendMode, SendPolicyDecision, SendPolicyInput, audit_event_for, evaluate,
};
use std::str::FromStr;

use super::attachments::{attachment_summaries, snapshot_attachments};
use super::common::setup_credentials;
use super::datetime::parse_send_at;
use super::drafts::{
    SentMailProofUi, persist_from_override, resolve_sent_copy_after_send,
    sent_copy_convenience_objects, sent_mail_proof_json, validate_from_override,
};
use super::governor_gate::{
    account_domain, gate_and_record, governor_request, precheck_attribution,
};
use super::re_subject_guard::check_new_re_subject_guard;
use super::ui;

/// Build lightweight attachment metadata (filename + content type, no bytes) for
/// the attribution precheck. The full bytes are only read later on an actual
/// transmit; attribution needs only the count and filename classification.
fn attachment_metadata(attach_paths: &[String]) -> Vec<Attachment> {
    attach_paths
        .iter()
        .map(|p| {
            let path = std::path::Path::new(p);
            let filename = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("attachment")
                .to_string();
            let content_type = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            Attachment {
                filename,
                content_type,
                data: Vec::new(),
            }
        })
        .collect()
}

/// Send an email immediately, or schedule it for later with `--at`.
#[tokio::main]
pub async fn run(
    to: &str,
    subject: &str,
    body: Option<&str>,
    html: Option<&str>,
    from: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    reply_to: Option<&str>,
    attach_paths: &[String],
    attr: &[String],
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
    at: Option<&str>,
    send_mode: &str,
    confirm_send: bool,
    allow_recipients: &[String],
    confirm_new_re_subject: bool,
    cooldown_seconds: Option<i64>,
    send_now: bool,
    confirm_send_now: bool,
) -> Result<()> {
    check_new_re_subject_guard(Some(subject), false, confirm_new_re_subject, json)?;

    let (db, creds) = setup_credentials(account, backend)?;
    let from = validate_from_override(from)?;
    let mode = SendMode::from_str(send_mode).map_err(|e| anyhow::anyhow!(e))?;
    let policy_input = SendPolicyInput {
        to,
        cc,
        bcc,
        confirm_send,
        allow_recipients,
    };
    let decision = evaluate(mode, &policy_input);
    record_send_policy_event(&db, &creds.account.id, mode, &decision, &policy_input);

    match &decision {
        SendPolicyDecision::Allowed => {}
        SendPolicyDecision::DraftOnly => {
            let draft_attachments = snapshot_attachments(attach_paths)?;
            let draft = db
                .create_draft(
                    &creds.account.id,
                    to,
                    Some(subject),
                    body,
                    html,
                    None,
                    cc,
                    bcc,
                    Some("cli"),
                )
                .context("failed to create send-policy draft")?;
            if !draft_attachments.is_empty() {
                db.update_draft_attachments(&draft.id, &draft_attachments)
                    .context("failed to persist draft attachments")?;
            }
            persist_from_override(&db, &draft.id, from)?;
            let attachment_summary = attachment_summaries(&draft_attachments);
            if json {
                println!(
                    "{}",
                    crate::commands::contract::send_body::cli_drafted(
                        serde_json::json!(mode),
                        &draft.id,
                        to,
                        subject,
                        serde_json::json!(attachment_summary),
                        ui::draft_ui(&creds.account.id, &draft.id),
                    )
                );
            } else {
                println!(
                    "Drafted instead of sending ({mode}). Draft ID: {}",
                    draft.id
                );
                if !attachment_summary.is_empty() {
                    println!("Attachments: {}", attachment_summary.len());
                    for a in &attachment_summary {
                        println!(
                            "  - {} ({} bytes, {})",
                            a["filename"].as_str().unwrap_or("attachment"),
                            a["size"].as_u64().unwrap_or(0),
                            a["content_type"]
                                .as_str()
                                .unwrap_or("application/octet-stream"),
                        );
                    }
                }
            }
            return Ok(());
        }
        SendPolicyDecision::Denied(denial) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "denied",
                        "error": denial,
                        "send_mode": mode,
                        "ui": ui::account_ui(&creds.account.id),
                    })
                );
            }
            anyhow::bail!("send denied by policy: {} ({})", denial.reason, denial.code);
        }
    }

    // ── Attribution precheck (before ANY side effect) ──
    //
    // A bot-originated send must carry at least one factual declared attribute.
    // This runs before the scheduled/queued draft is ever created, so a missing
    // or invalid declaration produces the canonical recovery payload with no
    // draft, no SMTP, and no Governor spawn.
    let declared: Vec<String> = attr.to_vec();
    let precheck_attachments = attachment_metadata(attach_paths);
    let precheck_req = governor_request(
        &creds.account.id,
        account_domain(&creds.account.username),
        subject,
        to,
        cc,
        bcc,
        SendSurface::Cli,
        None,
        &precheck_attachments,
        false,
        body,
        html,
        &declared,
    );
    if let Some(outcome) = precheck_attribution(&db, &creds.account.id, &precheck_req, None) {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "status": outcome.status_str(),
                    "error": outcome.error_json(),
                    "ui": ui::account_ui(&creds.account.id),
                })
            );
        }
        anyhow::bail!("{}", outcome.reason_string());
    }

    // The validated resolution for the additive success `attribution` block. On
    // queued/scheduled acceptance the real Governor decision runs later at the
    // sweep, so the block is marked deferred (governor null).
    let queued_attribution = precheck_req
        .resolution
        .as_ref()
        .map(|r| success_attribution_block(r, None, None, true));

    // The validated declaration is bound to the queued/scheduled draft in ONE
    // atomic store CAS (declaration + schedule + due status) via
    // `queue_bot_draft_for_send`, so the sweep gates on the SAME declaration the
    // bot just validated and no partial schedule can survive.

    // ── Scheduled send path ──
    if let Some(at_str) = at {
        let send_at = parse_send_at(at_str).context("failed to parse --at value")?;

        // Snapshot attachment bytes at schedule time so delivery does not depend
        // on the original files surviving. Bytes are base64-encoded into the
        // draft's attachments JSON. If a file is unreadable now, fail explicitly
        // rather than scheduling a send that silently drops the attachment.
        let scheduled_attachments = snapshot_attachments(attach_paths)?;

        // Create a draft with send_after set
        let draft = db
            .create_draft(
                &creds.account.id,
                to,
                Some(subject),
                body,
                html,
                None, // in_reply_to
                cc,
                bcc,
                Some("cli"),
            )
            .context("failed to create scheduled draft")?;

        if !scheduled_attachments.is_empty() {
            db.update_draft_attachments(&draft.id, &scheduled_attachments)
                .context("failed to persist scheduled attachments")?;
        }
        persist_from_override(&db, &draft.id, from)?;
        // One atomic CAS at the draft's final revision (attachments bumped it):
        // bind the declaration, set the schedule, and leave it at the due `draft`
        // status together — no partial schedule, no stale declaration.
        let revision = db
            .get_draft(&draft.id)
            .context("failed to reload scheduled draft")?
            .map(|d| d.revision)
            .ok_or_else(|| anyhow::anyhow!("scheduled draft vanished: {}", draft.id))?;
        crate::commands::drafts::queue_bot_draft_for_send(
            &db, &draft.id, revision, &send_at, &declared,
        )?;

        if json {
            println!(
                "{}",
                crate::commands::contract::send_body::cli_scheduled_at(
                    &draft.id,
                    &send_at,
                    serde_json::json!(attachment_summaries(&scheduled_attachments)),
                    serde_json::json!(queued_attribution),
                    ui::draft_ui(&creds.account.id, &draft.id),
                )
            );
        } else {
            println!("Scheduled for {send_at}. Draft ID: {}", draft.id);
            if !scheduled_attachments.is_empty() {
                println!("Attachments: {}", scheduled_attachments.len());
                for a in &scheduled_attachments {
                    println!(
                        "  - {} ({} bytes, {})",
                        a["filename"].as_str().unwrap_or("attachment"),
                        a["size"].as_u64().unwrap_or(0),
                        a["content_type"]
                            .as_str()
                            .unwrap_or("application/octet-stream"),
                    );
                }
            }
        }

        return Ok(());
    }

    // ── Default actual-send cooldown (outbox queueing) ──
    //
    // An allowed send does NOT transmit immediately. By default it queues into
    // the existing scheduled-send / outbox mechanism with a cooldown, and real
    // SMTP only happens later when the scheduled-send sweep finds it due (and
    // only after the Governor gate permits it). Immediate transmission is an
    // explicit, confirmed emergency bypass.
    let cooldown = resolve_cooldown_seconds(cooldown_seconds);
    match resolve_disposition(cooldown, send_now, confirm_send_now) {
        SendDisposition::NeedsConfirmation => {
            let denial = serde_json::json!({
                "code": IMMEDIATE_SEND_CONFIRM_CODE,
                "reason": "immediate send bypasses the outbox cooldown; pass --send-now together with --confirm-send-now",
            });
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "denied",
                        "error": denial,
                        "ui": ui::account_ui(&creds.account.id),
                    })
                );
            }
            anyhow::bail!(
                "immediate send requires confirmation: pass --send-now together with --confirm-send-now"
            );
        }
        SendDisposition::Queue {
            cooldown_seconds: cd,
        } => {
            let queued_attachments = snapshot_attachments(attach_paths)?;
            let draft = db
                .create_draft(
                    &creds.account.id,
                    to,
                    Some(subject),
                    body,
                    html,
                    None,
                    cc,
                    bcc,
                    Some("cli"),
                )
                .context("failed to create queued (cooldown) draft")?;
            if !queued_attachments.is_empty() {
                db.update_draft_attachments(&draft.id, &queued_attachments)
                    .context("failed to persist queued attachments")?;
            }
            persist_from_override(&db, &draft.id, from)?;
            let send_at = (chrono::Utc::now() + chrono::Duration::seconds(cd))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            // One atomic CAS at the draft's final revision (attachments bumped it):
            // declaration + schedule + due status together, or nothing.
            let revision = db
                .get_draft(&draft.id)
                .context("failed to reload queued draft")?
                .map(|d| d.revision)
                .ok_or_else(|| anyhow::anyhow!("queued draft vanished: {}", draft.id))?;
            crate::commands::drafts::queue_bot_draft_for_send(
                &db, &draft.id, revision, &send_at, &declared,
            )?;

            if json {
                println!(
                    "{}",
                    crate::commands::contract::send_body::cli_queued(
                        serde_json::json!(mode),
                        &draft.id,
                        &send_at,
                        cd,
                        OUTBOX_COOLDOWN_REASON_CODE,
                        OUTBOX_COOLDOWN_REASON,
                        serde_json::json!(attachment_summaries(&queued_attachments)),
                        serde_json::json!(queued_attribution),
                        ui::draft_ui(&creds.account.id, &draft.id),
                    )
                );
            } else {
                println!(
                    "Queued for send after {cd}s cooldown (at {send_at}). Draft ID: {}",
                    draft.id
                );
                println!("Reason: {OUTBOX_COOLDOWN_REASON}");
                println!(
                    "Real send happens via the scheduled-send sweep, after the Governor gate."
                );
            }
            return Ok(());
        }
        SendDisposition::Immediate => {
            // Explicit confirmed bypass — fall through to immediate send, but
            // only after the Governor gate permits it (below).
        }
    }

    // ── Immediate send path (explicit confirmed bypass) ──

    // Load each --attach file into memory
    let mut attachments: Vec<Attachment> = Vec::with_capacity(attach_paths.len());
    for path_str in attach_paths {
        let path = std::path::Path::new(path_str);
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("attachment")
            .to_string();
        let data = std::fs::read(path)
            .with_context(|| format!("failed to read attachment: {path_str}"))?;
        let content_type = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        attachments.push(Attachment {
            filename,
            content_type,
            data,
        });
    }

    // ── Governor gate (fail-closed before any real SMTP) ──
    let gov_req = governor_request(
        &creds.account.id,
        account_domain(&creds.account.username),
        subject,
        to,
        cc,
        bcc,
        SendSurface::Cli,
        None,
        &attachments,
        false,
        body,
        html,
        &declared,
    );
    let gov_outcome = gate_and_record(&db, &creds.account.id, &gov_req);
    if !gov_outcome.allowed {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "status": gov_outcome.status_str(),
                    "error": gov_outcome.error_json(),
                    "ui": ui::account_ui(&creds.account.id),
                })
            );
        }
        anyhow::bail!("{}", gov_outcome.reason_string());
    }

    let message_id = SmtpSender::send(
        &creds,
        to,
        subject,
        body,
        html,
        from,
        cc,
        bcc,
        reply_to,
        None, // in_reply_to — not a reply
        None, // references — not a reply
        &attachments,
    )
    .await
    .context("failed to send email")?;

    // Resolve Sent-folder copy using pre-append lookup semantics (issue #77).
    // Pre-lookup runs first: if the provider already filed the message, skip
    // the client IMAP APPEND so Gmail-style providers don't get duplicates.
    let from_for_sent = if let Some(f) = from {
        f.to_string()
    } else {
        super::drafts::account_from_header(&creds)
    };
    let provider_type = db.get_provider_type(&creds.account.id).ok().flatten();
    let copy_result = resolve_sent_copy_after_send(
        &db,
        &creds,
        provider_type.as_deref(),
        &from_for_sent,
        to,
        subject,
        body,
        html,
        cc,
        bcc,
        reply_to,
        None,
        &[],
        &message_id,
        &attachments,
    )
    .await;

    let sent_mail_appended = copy_result.sent_mail_appended;
    let sent_mail_append_skipped_reason = copy_result.sent_mail_append_skipped_reason;
    let sent_mail_proof = copy_result.proof;
    let (provider_sent_copy, client_appended_copy) =
        sent_copy_convenience_objects(&creds.account.id, &sent_mail_proof);
    let sent_message_url = sent_mail_proof.message_url(&creds.account.id);
    let sent_ui = sent_mail_proof.ui(&creds.account.id);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "sent",
                "to": to,
                "subject": subject,
                "message_id": message_id,
                "sent_mail_appended": sent_mail_appended,
                "sent_mail_append_skipped_reason": sent_mail_append_skipped_reason,
                "sent_folder": sent_mail_proof.folder.clone(),
                "sent_uid": sent_mail_proof.uid,
                "sent_message_url": sent_message_url,
                "sent_mail": sent_mail_proof_json(&creds.account.id, &sent_mail_proof),
                "provider_sent_copy": provider_sent_copy,
                "client_appended_copy": client_appended_copy,
                "attribution": gov_outcome.success_attribution(),
                "attachments": attachments.iter().map(|a| serde_json::json!({
                    "filename": a.filename,
                    "content_type": a.content_type,
                    "size": a.data.len(),
                })).collect::<Vec<_>>(),
                "ui": sent_ui,
            })
        );
    } else {
        println!("Sent to {to}");
        println!("Subject: {subject}");
        println!("Message-ID: {message_id}");
        match (sent_mail_proof.folder.as_deref(), sent_mail_proof.uid) {
            (Some(folder), Some(uid)) => {
                println!("Sent UID: {uid} ({folder})");
                if let Some(url) = sent_mail_proof.message_url(&creds.account.id) {
                    println!("Sent URL: {url}");
                }
            }
            (Some(folder), None) => println!(
                "Sent UID: unavailable in {folder} ({})",
                sent_mail_proof.lookup_status
            ),
            (None, None) => println!("Sent UID: unavailable ({})", sent_mail_proof.lookup_status),
            (None, Some(uid)) => println!("Sent UID: {uid}"),
        }
        if !attachments.is_empty() {
            println!("Attachments: {}", attachments.len());
            for a in &attachments {
                println!(
                    "  - {} ({} bytes, {})",
                    a.filename,
                    a.data.len(),
                    a.content_type
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::commands::drafts::{
        SentMailProof, provider_auto_saves_sent, sent_copy_convenience_objects,
    };

    // Regression: CLI immediate send must call resolve_sent_copy_after_send
    // (pre-lookup before append), not the old append helper directly.
    #[test]
    fn cli_send_no_longer_calls_append_helper_directly() {
        let src = include_str!("send.rs");
        let old_helper = concat!("append_sent_copy_for_immediate_", "send");
        assert!(
            !src.contains(old_helper),
            "CLI immediate send must go through resolve_sent_copy_after_send so pre-append lookup runs first"
        );
        assert!(src.contains("resolve_sent_copy_after_send"));
    }

    #[test]
    fn cli_send_json_output_shape_includes_sent_copy_source_fields() {
        // Simulate the proof that resolve_sent_copy_after_send would return for a
        // provider-auto-save path (e.g. Gmail). provider_sent_copy should be Some,
        // client_appended_copy should be None. Uses the shared projection the CLI
        // send path actually calls, so the two can never drift.
        let mut proof = SentMailProof::new(Some("Sent Mail".to_string()), Some(42), "found", None);
        proof.copy_source = "provider";

        let (provider_sent_copy, client_appended_copy) =
            sent_copy_convenience_objects("acct@example.com", &proof);

        assert!(
            provider_sent_copy.is_some(),
            "provider path: provider_sent_copy must be Some"
        );
        assert!(
            client_appended_copy.is_none(),
            "provider path: client_appended_copy must be None"
        );
        assert_eq!(
            provider_sent_copy.as_ref().unwrap()["copy_source"],
            "provider"
        );
    }

    #[test]
    fn cli_send_client_appended_path_populates_client_appended_copy() {
        let mut proof = SentMailProof::new(Some("Sent".to_string()), Some(99), "found", None);
        proof.copy_source = "client_appended";

        let (provider_sent_copy, client_appended_copy) =
            sent_copy_convenience_objects("acct@example.com", &proof);

        assert!(
            provider_sent_copy.is_none(),
            "client_appended path: provider_sent_copy must be None"
        );
        assert!(
            client_appended_copy.is_some(),
            "client_appended path: client_appended_copy must be Some"
        );
        assert_eq!(
            client_appended_copy.as_ref().unwrap()["copy_source"],
            "client_appended"
        );
    }

    #[test]
    fn cli_send_unresolved_never_reports_provider_sent_copy() {
        // Blocker regression: a generic-provider APPEND failure resolves as
        // `unresolved`; the CLI send output must not present it as provider proof.
        let mut proof = SentMailProof::new(Some("Sent".to_string()), None, "not_found", None);
        proof.copy_source = "unresolved";

        let (provider_sent_copy, client_appended_copy) =
            sent_copy_convenience_objects("acct@example.com", &proof);

        assert!(
            provider_sent_copy.is_none(),
            "unresolved must never be presented as provider_sent_copy"
        );
        assert!(client_appended_copy.is_none());
    }

    #[test]
    fn provider_auto_saves_sent_is_accessible_from_send_module() {
        // Verify the send module can access provider detection (used by
        // resolve_sent_copy_after_send for pre-lookup routing).
        assert!(provider_auto_saves_sent(Some("gmail"), "smtp.gmail.com"));
        assert!(!provider_auto_saves_sent(None, "smtp.migadu.com"));
    }

    #[test]
    fn every_new_draft_send_path_persists_explicit_from_identity() {
        let src = include_str!("send.rs");
        let obsolete_rejection = concat!("scheduled send does not persist sender ", "override yet");
        assert!(
            !src.contains(obsolete_rejection),
            "scheduled sends must accept a validated --from override"
        );
        let persistence_call = concat!("persist_from_override", "(&db, &draft.id, from)?;");
        assert_eq!(
            src.matches(persistence_call).count(),
            3,
            "draft-only, scheduled, and cooldown queue paths must all persist From"
        );
    }
}

fn record_send_policy_event(
    db: &Database,
    account_id: &str,
    mode: SendMode,
    decision: &SendPolicyDecision,
    input: &SendPolicyInput<'_>,
) {
    let audit = audit_event_for(mode, decision, input);
    let event = Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: account_id.to_string(),
        event_type: audit.event.to_string(),
        folder: "policy".to_string(),
        uid: None,
        message_id: None,
        from_addr: None,
        subject: None,
        snippet: None,
        payload: Some(audit.payload.to_string()),
        idempotency_key: None,
        secure_pending: false,
        acked_at: Some(chrono::Utc::now().to_rfc3339()),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let _ = db.insert_event(&event);
}
