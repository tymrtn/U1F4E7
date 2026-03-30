// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_store::CredentialBackend;
use envelope_email_transport::provider;

use super::common::setup_credentials;

#[tokio::main]
pub async fn run(account: Option<&str>, json: bool, backend: CredentialBackend) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    if json {
        // Use the provider-aware folder classification for rich JSON output
        let folder_infos = envelope_email_transport::folders::classify_folders(
            &mut client,
            &db,
            &creds.account.id,
        )
        .await
        .map_err(|e| anyhow::anyhow!("folder classification failed: {e}"))?;

        let items: Vec<serde_json::Value> = folder_infos
            .iter()
            .map(|fi| {
                let canonical = if fi.folder_type != "other" {
                    Some(fi.folder_type.as_str())
                } else {
                    provider::classify_folder(&fi.name)
                };
                serde_json::json!({
                    "name": fi.name,
                    "type": fi.folder_type,
                    "provider": fi.provider_type,
                    "canonical_name": canonical,
                })
            })
            .collect();

        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        let folders = envelope_email_transport::imap::list_folders(&mut client).await?;

        for folder in &folders {
            println!("{folder}");
        }
        println!("\n{} folder(s)", folders.len());
    }

    Ok(())
}
