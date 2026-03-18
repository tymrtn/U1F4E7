// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_transport::SmtpSender;

use super::common::setup_credentials;

#[tokio::main]
pub async fn run(
    to: &str,
    subject: &str,
    body: Option<&str>,
    html: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    reply_to: Option<&str>,
    account: Option<&str>,
    json: bool,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account)?;

    let message_id = SmtpSender::send(&creds, to, subject, body, html, cc, bcc, reply_to)
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
            })
        );
    } else {
        println!("Sent to {to}");
        println!("Subject: {subject}");
        println!("Message-ID: {message_id}");
    }

    Ok(())
}
