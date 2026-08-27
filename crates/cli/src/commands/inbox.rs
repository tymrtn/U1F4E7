// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_store::CredentialBackend;

use super::common::setup_credentials;
use super::ui;

#[tokio::main]
pub async fn run(
    folder: &str,
    limit: u32,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let messages = envelope_email_transport::imap::fetch_inbox(&mut client, folder, limit).await?;

    if json {
        let account_id = creds.account.id.as_str();
        let enriched: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| ui::with_ui(m, ui::message_or_draft_ui(&db, account_id, m.uid, folder)))
            .collect();
        println!("{}", serde_json::to_string_pretty(&enriched)?);
    } else {
        if messages.is_empty() {
            println!("No messages in {folder}");
            return Ok(());
        }

        println!(
            "{:<8}  {:<30}  {:<50}  {:<20}  {}",
            "UID", "FROM", "SUBJECT", "DATE", "FLAGS"
        );
        println!("{}", "-".repeat(120));
        for msg in &messages {
            let date = msg.date.as_deref().unwrap_or("-");
            let flags = msg.flags.join(", ");
            let subject = if msg.subject.len() > 48 {
                format!("{}...", &msg.subject[..48])
            } else {
                msg.subject.clone()
            };
            let from = if msg.from_addr.len() > 28 {
                format!("{}...", &msg.from_addr[..28])
            } else {
                msg.from_addr.clone()
            };
            println!(
                "{:<8}  {:<30}  {:<50}  {:<20}  {}",
                msg.uid, from, subject, date, flags,
            );
        }
        println!("\n{} message(s)", messages.len());
    }

    Ok(())
}
