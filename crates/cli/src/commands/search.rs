// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::CredentialBackend;
use envelope_email_store::models::MessageSummary;

use super::common::setup_credentials;
use super::ui;

/// Canonical folder roles accepted by `--role`/`--roles`. Mirrors the
/// classifications produced by `provider::classify_folder`.
const KNOWN_ROLES: &[&str] = &[
    "inbox", "drafts", "sent", "trash", "spam", "archive", "starred",
];

#[tokio::main]
pub async fn run(
    query: &str,
    folder: &str,
    limit: u32,
    account: Option<&str>,
    roles: &[String],
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;
    let account_id = creds.account.id.clone();

    // Resolve which folders to search. Without --role this is just the literal
    // --folder; with --role we map each requested role to every matching folder
    // for this account's provider layout. Search is read-only either way.
    let folders = if roles.is_empty() {
        vec![folder.to_string()]
    } else {
        resolve_role_folders(&mut client, &db, &account_id, roles).await?
    };

    // Collect (folder, message) pairs so output can attribute every hit to its
    // source folder — required when searching multiple role folders.
    let mut hits: Vec<(String, MessageSummary)> = Vec::new();
    for f in &folders {
        let messages = envelope_email_transport::imap::search(&mut client, f, query, limit).await?;
        for m in messages {
            hits.push((f.clone(), m));
        }
    }

    if json {
        let enriched: Vec<serde_json::Value> = hits
            .iter()
            .map(|(f, m)| ui::with_ui(m, ui::message_ui(&account_id, m.uid, f)))
            .collect();
        println!("{}", serde_json::to_string_pretty(&enriched)?);
    } else {
        if hits.is_empty() {
            if roles.is_empty() {
                println!("No messages matching: {query}");
            } else {
                println!(
                    "No messages matching: {query} (roles: {}, folders: {})",
                    roles.join(","),
                    folders.join(", ")
                );
            }
            return Ok(());
        }

        if !roles.is_empty() {
            println!(
                "Searched roles {} -> folders: {}",
                roles.join(","),
                folders.join(", ")
            );
        }
        println!(
            "{:<8}  {:<22}  {:<28}  {:<40}  {:<18}  FLAGS",
            "UID", "FOLDER", "FROM", "SUBJECT", "DATE"
        );
        println!("{}", "-".repeat(130));
        for (f, msg) in &hits {
            let date = msg.date.as_deref().unwrap_or("-");
            let flags = msg.flags.join(", ");
            let subject = truncate(&msg.subject, 38);
            let from = truncate(&msg.from_addr, 26);
            let folder_col = truncate(f, 20);
            println!(
                "{:<8}  {:<22}  {:<28}  {:<40}  {:<18}  {}",
                msg.uid, folder_col, from, subject, date, flags,
            );
        }
        println!("\n{} result(s)", hits.len());
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let mut t: String = s.chars().take(max).collect();
        t.push_str("...");
        t
    } else {
        s.to_string()
    }
}

/// Map requested roles to concrete folders for this account. Returns a clear
/// error (rather than silently empty results) when a role resolves to zero
/// folders, per issue #38 safety requirements.
async fn resolve_role_folders(
    client: &mut envelope_email_transport::imap::ImapClient,
    db: &envelope_email_store::Database,
    account_id: &str,
    roles: &[String],
) -> Result<Vec<String>> {
    // Validate role names up front.
    let mut normalized = Vec::new();
    for raw in roles {
        let role = raw.trim().to_lowercase();
        if !KNOWN_ROLES.contains(&role.as_str()) {
            bail!(
                "unknown folder role {raw:?}; known roles: {}",
                KNOWN_ROLES.join(", ")
            );
        }
        if !normalized.contains(&role) {
            normalized.push(role);
        }
    }

    let folder_infos = envelope_email_transport::folders::classify_folders(client, db, account_id)
        .await
        .map_err(|e| anyhow::anyhow!("folder classification failed: {e}"))?;

    let mut resolved: Vec<String> = Vec::new();
    let mut unmatched: Vec<String> = Vec::new();
    for role in &normalized {
        let matches: Vec<&str> = folder_infos
            .iter()
            .filter(|fi| &fi.folder_type == role)
            .map(|fi| fi.name.as_str())
            .collect();
        if matches.is_empty() {
            unmatched.push(role.clone());
        }
        for m in matches {
            if !resolved.iter().any(|r| r == m) {
                resolved.push(m.to_string());
            }
        }
    }

    if !unmatched.is_empty() {
        bail!(
            "no folders found for role(s): {} (account has: {})",
            unmatched.join(", "),
            folder_infos
                .iter()
                .map(|fi| format!("{}={}", fi.name, fi.folder_type))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    Ok(resolved)
}
