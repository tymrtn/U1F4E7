// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::{CredentialBackend, models::Message};

use super::common::setup_credentials;
use super::ui;

#[tokio::main]
pub async fn run(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account, backend)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let message = envelope_email_transport::imap::fetch_message(&mut client, folder, uid).await?;

    match message {
        Some(msg) => {
            if json {
                println!(
                    "{}",
                    serialize_message_json(&msg, &creds.account.id, folder)?
                );
            } else {
                println!("From: {}", msg.from_addr);
                println!("To: {}", msg.to_addr);
                if let Some(ref cc) = msg.cc_addr {
                    println!("Cc: {cc}");
                }
                println!("Subject: {}", msg.subject);
                if let Some(ref date) = msg.date {
                    println!("Date: {date}");
                }
                println!("Flags: {}", msg.flags.join(", "));
                println!();

                if let Some(ref text) = msg.text_body {
                    println!("{text}");
                } else if let Some(ref html) = msg.html_body {
                    println!("[HTML body — use --json for full content]");
                    println!("{html}");
                } else {
                    println!("[no body]");
                }
            }
        }
        None => bail!("message UID {uid} not found in {folder}"),
    }

    Ok(())
}

fn serialize_message_json(
    message: &Message,
    account_id: &str,
    folder: &str,
) -> serde_json::Result<String> {
    let value = ui::with_ui(message, ui::message_ui(account_id, message.uid, folder));
    serde_json::to_string_pretty(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_with_bodies(text_body: &str, html_body: &str) -> Message {
        Message {
            uid: 42,
            message_id: Some("<message@example.com>".to_string()),
            from_addr: "Sender <sender@example.com>".to_string(),
            to_addr: "Recipient <recipient@example.com>".to_string(),
            cc_addr: Some("Copy <copy@example.com>".to_string()),
            subject: "strict json body regression".to_string(),
            date: Some("2026-05-18T00:00:00+00:00".to_string()),
            text_body: Some(text_body.to_string()),
            html_body: Some(html_body.to_string()),
            in_reply_to: Some("<parent@example.com>".to_string()),
            references: Some("<root@example.com> <parent@example.com>".to_string()),
            flags: vec!["Seen".to_string()],
            attachments: Vec::new(),
        }
    }

    #[test]
    fn json_output_round_trips_message_bodies_with_control_chars() {
        let text_body = "first line\nsecond line\r\ncontains nul \0 and tab\t";
        let html_body = "<p>first</p>\n<script>\"quoted\"\0</script>";
        let message = message_with_bodies(text_body, html_body);

        let rendered = serialize_message_json(&message, "acct-1", "INBOX").unwrap();

        assert!(!rendered.contains('\0'));
        assert!(!rendered.contains('\r'));
        assert!(rendered.contains("\\n"));
        assert!(rendered.contains("\\r"));
        assert!(rendered.contains("\\t"));
        assert!(rendered.contains("\\u0000"));

        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["text_body"], text_body);
        assert_eq!(parsed["html_body"], html_body);
        assert!(parsed["ui"].is_object());
    }
}
