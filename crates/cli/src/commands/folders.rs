// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::collections::HashMap;

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

    // Use the provider-aware folder classification for rich output
    let folder_infos =
        envelope_email_transport::folders::classify_folders(&mut client, &db, &creds.account.id)
            .await
            .map_err(|e| anyhow::anyhow!("folder classification failed: {e}"))?;

    // Fetch stats for every folder (exists / recent / unseen)
    let stats_vec = envelope_email_transport::imap::list_folder_stats(&mut client)
        .await
        .context("folder_stats failed")?;
    let stats_map: HashMap<String, envelope_email_store::models::FolderStats> = stats_vec
        .into_iter()
        .map(|s| (s.folder.clone(), s))
        .collect();

    if json {
        let items: Vec<serde_json::Value> = folder_infos
            .iter()
            .map(|fi| {
                let canonical = if fi.folder_type != "other" {
                    Some(fi.folder_type.as_str())
                } else {
                    provider::classify_folder(&fi.name)
                };
                let stats = stats_map.get(&fi.name);
                serde_json::json!({
                    "name": fi.name,
                    "type": fi.folder_type,
                    "provider": fi.provider_type,
                    "canonical_name": canonical,
                    "exists": stats.map(|s| s.exists).unwrap_or(0),
                    "recent": stats.map(|s| s.recent).unwrap_or(0),
                    "unseen": stats.and_then(|s| s.unseen),
                })
            })
            .collect();

        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        // Text output: folder name + "(N unseen / M total)"
        let max_name_len = folder_infos
            .iter()
            .map(|fi| fi.name.len())
            .max()
            .unwrap_or(20);
        for fi in &folder_infos {
            let stats = stats_map.get(&fi.name);
            let exists = stats.map(|s| s.exists).unwrap_or(0);
            let unseen = stats.and_then(|s| s.unseen);
            let counts = match unseen {
                Some(u) if u > 0 => format!("{u} unseen / {exists} total"),
                _ => format!("{exists} total"),
            };
            println!("{:width$}  {}", fi.name, counts, width = max_name_len);
        }
        println!("\n{} folder(s)", folder_infos.len());
    }

    Ok(())
}
