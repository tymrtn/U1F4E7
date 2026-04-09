// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_store::CredentialBackend;
use envelope_email_transport::SmtpSender;
use envelope_email_transport::smtp::Attachment;

use super::common::setup_credentials;

/// Simple send — no attachments. Delegates to run_with_attachments with an empty slice.
#[tokio::main]
pub async fn run(
    to: &str,
    subject: &str,
    body: Option<&str>,
    html: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    reply_to: Option<&str>,
    attach_paths: &[String],
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account, backend)?;

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
            })
        );
    } else {
        println!("Sent to {to}");
        println!("Subject: {subject}");
        println!("Message-ID: {message_id}");
        if !attachments.is_empty() {
            println!("Attachments: {}", attachments.len());
            for a in &attachments {
                println!("  - {} ({} bytes, {})", a.filename, a.data.len(), a.content_type);
            }
        }
    }

    Ok(())
}
