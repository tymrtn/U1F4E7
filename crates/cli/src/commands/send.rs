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
use super::drafts::{find_sent_mail_by_message_id, sent_mail_proof_json};
use super::re_subject_guard::check_new_re_subject_guard;
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
    confirm_new_re_subject: bool,
) -> Result<()> {
    check_new_re_subject_guard(Some(subject), false, confirm_new_re_subject, json)?;

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
        if from.is_some() {
            anyhow::bail!(
                "--from is not supported with --at (scheduled send does not persist sender override yet)"
            );
        }
        let send_at = parse_until(at_str).context("failed to parse --at value")?;

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

        db.update_draft_send_after(&draft.id, &send_at)
            .context("failed to set send_after on draft")?;

        if !scheduled_attachments.is_empty() {
            db.update_draft_attachments(&draft.id, &scheduled_attachments)
                .context("failed to persist scheduled attachments")?;
        }

        if json {
            println!(
                "{}",
                serde_json::json!({
                    "scheduled": true,
                    "send_at": send_at,
                    "draft_id": draft.id,
                    "attachments": scheduled_attachments_summary(&scheduled_attachments),
                    "ui": ui::draft_ui(&creds.account.id, &draft.id),
                })
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

    let sent_mail_proof = find_sent_mail_by_message_id(&db, &creds, &message_id).await;
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
                "sent_folder": sent_mail_proof.folder.clone(),
                "sent_uid": sent_mail_proof.uid,
                "sent_message_url": sent_message_url,
                "sent_mail": sent_mail_proof_json(&creds.account.id, &sent_mail_proof),
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

/// Read each `--attach` file and snapshot its bytes into a JSON attachment
/// entry suitable for storage on a scheduled draft.
///
/// Each entry carries `filename`, `content_type`, `size`, and `data_base64`.
/// Returns an explicit error if any file cannot be read so a scheduled send is
/// never created with a silently-missing attachment.
fn snapshot_attachments(attach_paths: &[String]) -> Result<Vec<serde_json::Value>> {
    use base64::Engine as _;
    let mut out = Vec::with_capacity(attach_paths.len());
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
        let data_base64 = base64::engine::general_purpose::STANDARD.encode(&data);
        out.push(serde_json::json!({
            "filename": filename,
            "content_type": content_type,
            "size": data.len(),
            "data_base64": data_base64,
        }));
    }
    Ok(out)
}

/// Build a non-secret summary of scheduled attachments for JSON output.
/// Deliberately excludes `data_base64` so attachment bytes never appear in
/// command output, logs, or audit surfaces.
fn scheduled_attachments_summary(attachments: &[serde_json::Value]) -> Vec<serde_json::Value> {
    attachments
        .iter()
        .map(|a| {
            serde_json::json!({
                "filename": a["filename"],
                "content_type": a["content_type"],
                "size": a["size"],
            })
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn snapshot_attachments_encodes_bytes_and_metadata() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"hello").unwrap();
        let path = f.path().to_str().unwrap().to_string();

        let snap = snapshot_attachments(&[path]).unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0]["size"], 5);
        // "hello" base64-encoded
        assert_eq!(snap[0]["data_base64"], "aGVsbG8=");
        assert!(snap[0]["filename"].as_str().unwrap().len() > 0);
    }

    #[test]
    fn snapshot_attachments_errors_on_missing_file() {
        let err = snapshot_attachments(&["/no/such/path/at/all.txt".to_string()]).unwrap_err();
        assert!(err.to_string().contains("failed to read attachment"));
    }

    #[test]
    fn empty_attach_paths_snapshot_is_empty() {
        assert!(snapshot_attachments(&[]).unwrap().is_empty());
    }

    #[test]
    fn summary_excludes_attachment_bytes() {
        let attachments = vec![serde_json::json!({
            "filename": "secret.txt",
            "content_type": "text/plain",
            "size": 5,
            "data_base64": "aGVsbG8=",
        })];
        let summary = scheduled_attachments_summary(&attachments);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert!(!serialized.contains("data_base64"));
        assert!(!serialized.contains("aGVsbG8="));
        assert!(serialized.contains("secret.txt"));
        assert!(serialized.contains("text/plain"));
    }
}
