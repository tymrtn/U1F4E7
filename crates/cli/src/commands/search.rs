// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};

use super::common::setup_credentials;

#[tokio::main]
pub async fn run(
    query: &str,
    folder: &str,
    limit: u32,
    account: Option<&str>,
    json: bool,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let results =
        envelope_email_transport::imap::search(&mut client, folder, query, limit).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        if results.is_empty() {
            println!("No results for \"{query}\" in {folder}");
            return Ok(());
        }

        println!(
            "{:<8}  {:<30}  {:<50}  {}",
            "UID", "FROM", "SUBJECT", "DATE"
        );
        println!("{}", "-".repeat(110));
        for msg in &results {
            let date = msg.date.as_deref().unwrap_or("-");
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
            println!("{:<8}  {:<30}  {:<50}  {}", msg.uid, from, subject, date);
        }
        println!("\n{} result(s)", results.len());
    }

    Ok(())
}
