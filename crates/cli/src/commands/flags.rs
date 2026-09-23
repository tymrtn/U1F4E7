// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_store::CredentialBackend;

use super::common::setup_credentials;

#[tokio::main]
pub async fn run_add(
    uid: u32,
    flag: &str,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    envelope_email_transport::imap::set_flag(&mut client, folder, uid, flag).await?;
    envelope_email_transport::imap::record_own_flag_change(
        &db,
        &creds.account.id,
        folder,
        &[uid],
        flag,
        true,
    )
    .context("flag changed on the server, but updating the local message index failed")?;

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
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    envelope_email_transport::imap::remove_flag(&mut client, folder, uid, flag).await?;
    envelope_email_transport::imap::record_own_flag_change(
        &db,
        &creds.account.id,
        folder,
        &[uid],
        flag,
        false,
    )
    .context("flag changed on the server, but updating the local message index failed")?;

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
