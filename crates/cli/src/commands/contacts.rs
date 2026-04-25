// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::collections::HashMap;

use anyhow::{Context, Result};
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_store::models::Contact;
use envelope_email_transport::imap;

use super::common::setup_credentials;

/// `envelope contacts add` — add or update a contact.
pub fn run_add(
    email: &str,
    name: Option<&str>,
    tags: &[String],
    notes: Option<&str>,
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;
    let account_id = &acct.id;

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let tags_json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string());

    let contact = Contact {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: account_id.clone(),
        email: email.to_string(),
        name: name.map(|n| n.to_string()),
        tags: tags_json,
        notes: notes.map(|n| n.to_string()),
        message_count: 0,
        first_seen: None,
        last_seen: None,
        created_at: now.clone(),
        updated_at: now,
    };

    db.upsert_contact(&contact)
        .context("failed to upsert contact")?;

    if json {
        // Re-fetch to get the canonical record (upsert may have merged fields)
        let saved = db.get_contact(account_id, email)?;
        println!("{}", serde_json::to_string_pretty(&saved)?);
    } else {
        println!("Added contact: {email}");
        if let Some(n) = name {
            println!("  Name:  {n}");
        }
        if !tags.is_empty() {
            println!("  Tags:  {}", tags.join(", "));
        }
        if let Some(n) = notes {
            println!("  Notes: {n}");
        }
    }

    Ok(())
}

/// `envelope contacts list` — list contacts for an account.
pub fn run_list(
    tag_filter: Option<&str>,
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;

    let contacts = db
        .list_contacts(&acct.id, tag_filter)
        .context("failed to list contacts")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&contacts)?);
    } else {
        if contacts.is_empty() {
            println!("No contacts found");
            return Ok(());
        }

        println!(
            "{:<30}  {:<20}  {:<6}  {:<20}  {}",
            "EMAIL", "NAME", "MSGS", "LAST SEEN", "TAGS"
        );
        println!("{}", "-".repeat(100));
        for c in &contacts {
            let name = c.name.as_deref().unwrap_or("-");
            let name_display = if name.len() > 18 {
                format!("{}...", &name[..15])
            } else {
                name.to_string()
            };
            let email_display = if c.email.len() > 28 {
                format!("{}...", &c.email[..25])
            } else {
                c.email.clone()
            };
            let last_seen = c.last_seen.as_deref().unwrap_or("-");
            let last_seen_display = if last_seen.len() > 18 {
                &last_seen[..18]
            } else {
                last_seen
            };
            let tags: Vec<String> = serde_json::from_str(&c.tags).unwrap_or_default();
            let tags_display = if tags.is_empty() {
                "-".to_string()
            } else {
                tags.join(", ")
            };
            println!(
                "{:<30}  {:<20}  {:<6}  {:<20}  {}",
                email_display, name_display, c.message_count, last_seen_display, tags_display
            );
        }
        println!("\n{} contact(s)", contacts.len());
    }

    Ok(())
}

/// `envelope contacts show <email>` — show a single contact.
pub fn run_show(
    email: &str,
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;

    let contact = db
        .get_contact(&acct.id, email)
        .context("failed to get contact")?
        .ok_or_else(|| anyhow::anyhow!("contact not found: {email}"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&contact)?);
    } else {
        let tags: Vec<String> = serde_json::from_str(&contact.tags).unwrap_or_default();
        println!("Contact: {}", contact.email);
        println!("  ID:            {}", contact.id);
        println!(
            "  Name:          {}",
            contact.name.as_deref().unwrap_or("(none)")
        );
        println!(
            "  Tags:          {}",
            if tags.is_empty() {
                "(none)".to_string()
            } else {
                tags.join(", ")
            }
        );
        println!(
            "  Notes:         {}",
            contact.notes.as_deref().unwrap_or("(none)")
        );
        println!("  Messages:      {}", contact.message_count);
        println!(
            "  First seen:    {}",
            contact.first_seen.as_deref().unwrap_or("(never)")
        );
        println!(
            "  Last seen:     {}",
            contact.last_seen.as_deref().unwrap_or("(never)")
        );
        println!("  Created:       {}", contact.created_at);
        println!("  Updated:       {}", contact.updated_at);
    }

    Ok(())
}

/// `envelope contacts tag <email> --tag <tag>` — add a tag to a contact.
pub fn run_tag(
    email: &str,
    tag: &str,
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;

    let found = db
        .add_contact_tag(&acct.id, email, tag)
        .context("failed to add tag")?;

    if !found {
        anyhow::bail!("contact not found: {email}");
    }

    if json {
        println!(
            "{}",
            serde_json::json!({"action": "tag_added", "email": email, "tag": tag})
        );
    } else {
        println!("Added tag '{tag}' to {email}");
    }

    Ok(())
}

/// `envelope contacts untag <email> --tag <tag>` — remove a tag from a contact.
pub fn run_untag(
    email: &str,
    tag: &str,
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = envelope_email_store::Database::open_default().context("failed to open database")?;
    let acct = super::common::resolve_account(&db, account)?;

    let found = db
        .remove_contact_tag(&acct.id, email, tag)
        .context("failed to remove tag")?;

    if !found {
        anyhow::bail!("contact not found: {email}");
    }

    if json {
        println!(
            "{}",
            serde_json::json!({"action": "tag_removed", "email": email, "tag": tag})
        );
    } else {
        println!("Removed tag '{tag}' from {email}");
    }

    Ok(())
}

/// `envelope contacts import` — import contacts from inbox senders.
#[tokio::main]
pub async fn run_import_inbox(
    limit: u32,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let account_id = creds.account.id.clone();

    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let summaries = imap::fetch_inbox(&mut client, "INBOX", limit)
        .await
        .context("failed to fetch inbox messages")?;

    // Aggregate sender stats: (email -> (count, earliest_date, latest_date, name_guess))
    let mut sender_stats: HashMap<String, (i64, Option<String>, Option<String>)> = HashMap::new();

    for msg in &summaries {
        let from = msg.from_addr.trim().to_lowercase();
        if from.is_empty() {
            continue;
        }

        let entry = sender_stats.entry(from).or_insert((0, None, None));

        entry.0 += 1;

        // Track first_seen and last_seen from message dates
        if let Some(ref date) = msg.date {
            match &entry.1 {
                None => entry.1 = Some(date.clone()),
                Some(existing) => {
                    if date < existing {
                        entry.1 = Some(date.clone());
                    }
                }
            }
            match &entry.2 {
                None => entry.2 = Some(date.clone()),
                Some(existing) => {
                    if date > existing {
                        entry.2 = Some(date.clone());
                    }
                }
            }
        }
    }

    let mut created = 0u32;
    let mut updated = 0u32;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    for (email, (count, first_seen, last_seen)) in &sender_stats {
        let existing = db.get_contact(&account_id, email)?;

        let contact = match existing {
            Some(mut c) => {
                c.message_count = *count;
                if first_seen.is_some() {
                    c.first_seen = first_seen.clone();
                }
                if last_seen.is_some() {
                    c.last_seen = last_seen.clone();
                }
                c.updated_at = now.clone();
                updated += 1;
                c
            }
            None => {
                created += 1;
                Contact {
                    id: uuid::Uuid::new_v4().to_string(),
                    account_id: account_id.clone(),
                    email: email.clone(),
                    name: None,
                    tags: "[]".to_string(),
                    notes: None,
                    message_count: *count,
                    first_seen: first_seen.clone(),
                    last_seen: last_seen.clone(),
                    created_at: now.clone(),
                    updated_at: now.clone(),
                }
            }
        };

        db.upsert_contact(&contact)
            .with_context(|| format!("failed to upsert contact {email}"))?;
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "messages_scanned": summaries.len(),
                "unique_senders": sender_stats.len(),
                "contacts_created": created,
                "contacts_updated": updated,
            }))?
        );
    } else {
        println!(
            "Scanned {} messages, found {} unique senders",
            summaries.len(),
            sender_stats.len()
        );
        println!("  Created: {created}");
        println!("  Updated: {updated}");
    }

    Ok(())
}
