// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};

use super::common::setup_credentials;

#[tokio::main]
pub async fn run_move(
    uid: u32,
    folder: &str,
    to_folder: &str,
    account: Option<&str>,
    json: bool,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    envelope_email_transport::imap::move_message(&mut client, uid, folder, to_folder).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "move",
                "uid": uid,
                "from": folder,
                "to": to_folder,
            })
        );
    } else {
        println!("Moved UID {uid} from {folder} to {to_folder}");
    }

    Ok(())
}

#[tokio::main]
pub async fn run_copy(
    uid: u32,
    folder: &str,
    to_folder: &str,
    account: Option<&str>,
    json: bool,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    envelope_email_transport::imap::copy_message(&mut client, uid, folder, to_folder).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "copy",
                "uid": uid,
                "from": folder,
                "to": to_folder,
            })
        );
    } else {
        println!("Copied UID {uid} from {folder} to {to_folder}");
    }

    Ok(())
}

#[tokio::main]
pub async fn run_delete(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    json: bool,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    envelope_email_transport::imap::delete_message(&mut client, folder, uid).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "delete",
                "uid": uid,
                "folder": folder,
            })
        );
    } else {
        println!("Deleted UID {uid} from {folder}");
    }

    Ok(())
}
