// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::Database;
use envelope_email_store::credential_store::CredentialBackend;

use super::common::{resolve_account, setup_credentials};

/// `envelope-email thread <uid>` — show the full conversation thread for a message.
#[tokio::main]
pub async fn run_show(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;

    // First, try to find the thread by UID in our local DB
    let thread_id = db
        .find_thread_by_uid(uid, folder)
        .context("database error")?;

    let thread_id = match thread_id {
        Some(tid) => tid,
        None => {
            // Thread not found — try building threads first
            eprintln!("Thread data not found for UID {uid}. Building threads...");
            envelope_email_transport::threading::build_threads(&creds, &db, 200)
                .await
                .context("failed to build threads")?;

            // Try again
            match db
                .find_thread_by_uid(uid, folder)
                .context("database error")?
            {
                Some(tid) => tid,
                None => bail!(
                    "message UID {uid} in {folder} not found in any thread. \
                     Try building threads with: envelope-email thread build"
                ),
            }
        }
    };

    let thread = db
        .get_thread(&thread_id)
        .context("database error")?
        .context("thread not found in database")?;

    let messages = db
        .get_thread_messages(&thread_id)
        .context("failed to get thread messages")?;

    if json {
        let output = serde_json::json!({
            "thread_id": thread.thread_id,
            "subject": thread.subject_normalized,
            "message_count": thread.message_count,
            "first_seen": thread.first_seen,
            "last_activity": thread.last_activity,
            "account_id": thread.account_id,
            "messages": messages,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        // Pretty conversation view
        let subject_display = if messages.is_empty() {
            thread.subject_normalized.clone()
        } else {
            messages
                .last()
                .and_then(|m| m.subject.clone())
                .unwrap_or_else(|| thread.subject_normalized.clone())
        };
        println!(
            "Thread: {} ({} message{})",
            subject_display,
            thread.message_count,
            if thread.message_count == 1 { "" } else { "s" },
        );
        println!();

        for msg in &messages {
            let direction = if msg.is_outbound { "→" } else { "←" };
            let date_short = format_short_date(msg.date.as_deref().unwrap_or(""));
            let from = msg.from_address.as_deref().unwrap_or("(unknown)");
            let to = msg.to_addresses.as_deref().unwrap_or("(unknown)");

            println!("{} [{}] {} → {}", direction, date_short, from, to,);
            if let Some(ref snippet) = msg.snippet {
                // Indent snippet
                for line in snippet.lines().take(3) {
                    println!("  {line}");
                }
            }
            println!();
        }
    }

    Ok(())
}

/// `envelope-email thread list` — list recent threads.
#[tokio::main]
pub async fn run_list(
    account: Option<&str>,
    limit: u32,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    let account_id = if let Some(acct) = account {
        let resolved = resolve_account(&db, Some(acct))?;
        Some(resolved.id)
    } else {
        None
    };

    let threads = db
        .list_threads(account_id.as_deref(), limit)
        .context("failed to list threads")?;

    if json {
        // Enrich each thread with participant info
        let mut enriched = Vec::new();
        for thread in &threads {
            let messages = db
                .get_thread_messages(&thread.thread_id)
                .unwrap_or_default();
            let participants: std::collections::HashSet<String> = messages
                .iter()
                .filter_map(|m| m.from_address.as_ref().map(|s| s.to_lowercase()))
                .collect();

            enriched.push(serde_json::json!({
                "thread_id": thread.thread_id,
                "subject": thread.subject_normalized,
                "message_count": thread.message_count,
                "participant_count": participants.len(),
                "participants": participants.into_iter().collect::<Vec<_>>(),
                "first_seen": thread.first_seen,
                "last_activity": thread.last_activity,
                "account_id": thread.account_id,
            }));
        }
        println!("{}", serde_json::to_string_pretty(&enriched)?);
    } else {
        if threads.is_empty() {
            println!("No threads found. Build threads with: envelope-email thread build");
            return Ok(());
        }

        println!(
            "{:<38}  {:<40}  {:>5}  {:<20}",
            "THREAD ID", "SUBJECT", "MSGS", "LAST ACTIVITY"
        );
        println!("{}", "-".repeat(110));

        for thread in &threads {
            let subject = if thread.subject_normalized.len() > 38 {
                format!("{}...", &thread.subject_normalized[..38])
            } else {
                thread.subject_normalized.clone()
            };
            let last = format_short_date(&thread.last_activity);

            println!(
                "{:<38}  {:<40}  {:>5}  {:<20}",
                &thread.thread_id[..8], // Show first 8 chars of UUID
                subject,
                thread.message_count,
                last,
            );
        }
        println!("\n{} thread(s)", threads.len());
    }

    Ok(())
}

/// `envelope-email thread build` — build/rebuild threads for an account.
#[tokio::main]
pub async fn run_build(
    account: Option<&str>,
    limit: u32,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;

    if !json {
        eprintln!(
            "Building threads for {} (scanning up to {limit} messages per folder)...",
            creds.account.username,
        );
    }

    let result = envelope_email_transport::threading::build_threads(&creds, &db, limit)
        .await
        .context("failed to build threads")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Thread build complete:");
        println!("  Messages indexed: {}", result.messages_indexed);
        println!("  Threads created:  {}", result.threads_created);
        println!("  Threads updated:  {}", result.threads_updated);
        if let Some(ref sent) = result.sent_folder {
            println!("  Sent folder:      {sent}");
        }
    }

    Ok(())
}

/// Format an ISO8601 datetime into a short display format: "Mar 28 10:30"
fn format_short_date(iso: &str) -> String {
    use chrono::NaiveDateTime;

    // Try full RFC3339 first
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
        return dt.format("%b %d %H:%M").to_string();
    }

    // Try NaiveDateTime
    if let Ok(dt) = NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S") {
        return dt.format("%b %d %H:%M").to_string();
    }

    // Fallback: return as-is truncated
    iso.chars().take(16).collect()
}
