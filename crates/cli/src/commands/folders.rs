// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};

use super::common::setup_credentials;

#[tokio::main]
pub async fn run(account: Option<&str>, json: bool) -> Result<()> {
    let (_db, creds) = setup_credentials(account)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let folders = envelope_email_transport::imap::list_folders(&mut client).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&folders)?);
    } else {
        if folders.is_empty() {
            println!("No folders found");
            return Ok(());
        }

        for folder in &folders {
            println!("{folder}");
        }
        println!("\n{} folder(s)", folders.len());
    }

    Ok(())
}
