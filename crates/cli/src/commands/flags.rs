// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};

use super::common::setup_credentials;

#[tokio::main]
pub async fn run_add(
    uid: u32,
    flag: &str,
    folder: &str,
    account: Option<&str>,
    json: bool,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    envelope_email_transport::imap::set_flag(&mut client, folder, uid, flag).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "flag_add",
                "uid": uid,
                "flag": flag,
                "folder": folder,
            })
        );
    } else {
        println!("Added flag {flag} to UID {uid} in {folder}");
    }

    Ok(())
}

#[tokio::main]
pub async fn run_remove(
    uid: u32,
    flag: &str,
    folder: &str,
    account: Option<&str>,
    json: bool,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    envelope_email_transport::imap::remove_flag(&mut client, folder, uid, flag).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "flag_remove",
                "uid": uid,
                "flag": flag,
                "folder": folder,
            })
        );
    } else {
        println!("Removed flag {flag} from UID {uid} in {folder}");
    }

    Ok(())
}
