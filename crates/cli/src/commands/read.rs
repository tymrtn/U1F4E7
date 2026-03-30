// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::CredentialBackend;

use super::common::setup_credentials;

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
                println!("{}", serde_json::to_string_pretty(&msg)?);
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
