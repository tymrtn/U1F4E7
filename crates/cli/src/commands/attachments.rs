// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use envelope_email_store::CredentialBackend;

use super::common::setup_credentials;

/// List attachments for a message by UID.
#[tokio::main]
pub async fn run_list(
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
                println!("{}", serde_json::to_string_pretty(&msg.attachments)?);
            } else if msg.attachments.is_empty() {
                println!("No attachments for UID {uid} in {folder}");
            } else {
                println!("Attachments for UID {uid}:");
                for (i, att) in msg.attachments.iter().enumerate() {
                    println!(
                        "  {i}: {name}  ({ct}, {size} bytes)",
                        name = att.filename,
                        ct = att.content_type,
                        size = att.size,
                    );
                }
            }
        }
        None => bail!("message UID {uid} not found in {folder}"),
    }

    Ok(())
}

/// Download an attachment by filename from a message, saving to disk.
#[tokio::main]
pub async fn run_download(
    uid: u32,
    filename: &str,
    output: Option<&str>,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account, backend)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let (name, bytes) =
        envelope_email_transport::imap::download_attachment(&mut client, uid, filename, folder)
            .await
            .context("failed to download attachment")?;

    // Determine output path: explicit --output, or current directory + filename
    let dest = match output {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(&name),
    };

    std::fs::write(&dest, &bytes).with_context(|| format!("failed to write {}", dest.display()))?;

    if json {
        let info = serde_json::json!({
            "filename": name,
            "size": bytes.len(),
            "path": dest.display().to_string(),
        });
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!(
            "Saved {name} ({size} bytes) to {path}",
            size = bytes.len(),
            path = dest.display(),
        );
    }

    Ok(())
}
