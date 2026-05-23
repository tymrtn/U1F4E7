// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_store::{CredentialBackend, Database, Event};
use envelope_email_transport::SmtpSender;
use envelope_email_transport::smtp::Attachment;
use envelope_email_transport::{
    SendMode, SendPolicyDecision, SendPolicyInput, audit_event_for, evaluate,
};
use std::str::FromStr;

use super::common::setup_credentials;
use super::datetime::parse_until;
use super::ui;

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
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
    at: Option<&str>,
    send_mode: &str,
    confirm_send: bool,
    allow_recipients: &[String],
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
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
            if !attach_paths.is_empty() {
                anyhow::bail!(
                    "--attach is not supported with --send-mode draft-only (draft storage does not persist attachments yet)"
                );
            }
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
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "drafted",
                        "send_mode": mode,
                        "draft_id": draft.id,
                        "to": to,
                        "subject": subject,
                        "ui": ui::draft_ui(&creds.account.id, &draft.id),
                    })
                );
            } else {
                println!(
                    "Drafted instead of sending ({mode}). Draft ID: {}",
                    draft.id
                );
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

    // ── Scheduled send path ──
    if let Some(at_str) = at {
        if !attach_paths.is_empty() {
            anyhow::bail!(
                "--attach is not supported with --at (scheduled send does not persist attachments yet)"
            );
        }
        if from.is_some() {
            anyhow::bail!(
                "--from is not supported with --at (scheduled send does not persist sender override yet)"
            );
        }
        let send_at = parse_until(at_str).context("failed to parse --at value")?;

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

        db.update_draft_send_after(&draft.id, &send_at)
            .context("failed to set send_after on draft")?;

        if json {
            println!(
                "{}",
                serde_json::json!({
                    "scheduled": true,
                    "send_at": send_at,
                    "draft_id": draft.id,
                    "ui": ui::draft_ui(&creds.account.id, &draft.id),
                })
            );
        } else {
            println!("Scheduled for {send_at}. Draft ID: {}", draft.id);
        }

        return Ok(());
    }

    // ── Immediate send path (unchanged) ──

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

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "sent",
                "to": to,
                "subject": subject,
                "message_id": message_id,
                "attachments": attachments.iter().map(|a| serde_json::json!({
                    "filename": a.filename,
                    "content_type": a.content_type,
                    "size": a.data.len(),
                })).collect::<Vec<_>>(),
                "ui": ui::account_ui(&creds.account.id),
            })
        );
    } else {
        println!("Sent to {to}");
        println!("Subject: {subject}");
        println!("Message-ID: {message_id}");
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
