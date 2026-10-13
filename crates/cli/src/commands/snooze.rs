// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_store::{Database, SnoozedMessage};
use envelope_email_transport::errors::ImapError;
use envelope_email_transport::imap;
use tracing::{info, warn};

use super::agent_context;
use super::common::{resolve_account, setup_credentials};
use super::datetime::parse_until;

/// The default IMAP folder name for snoozed messages.
const IMAP_SNOOZED_FOLDER: &str = "Snoozed";

/// Format of a stored `return_at`: naive UTC, as `parse_until` writes it, so
/// due checks are plain string comparisons.
const RETURN_AT_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

/// Valid snooze reasons.
const VALID_REASONS: &[&str] = &["follow-up", "waiting-reply", "defer", "reminder", "review"];

/// Validate a snooze reason against the allowed set.
fn validate_reason(reason: &str) -> Result<()> {
    if VALID_REASONS.contains(&reason) {
        Ok(())
    } else {
        bail!(
            "invalid reason '{}'. Must be one of: {}",
            reason,
            VALID_REASONS.join(", ")
        )
    }
}

/// Snooze a message: move it to the Snoozed folder and record in local DB.
#[tokio::main]
pub async fn run_snooze(
    uid: u32,
    until: &str,
    folder: &str,
    account: Option<&str>,
    reason: Option<&str>,
    note: Option<&str>,
    recipient: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let return_at = parse_until(until).context("failed to parse --until value")?;

    // Validate reason if provided
    if let Some(r) = reason {
        validate_reason(r)?;
    }

    let (db, creds) = setup_credentials(account, backend)?;
    let account_email = &creds.account.username;
    let ctx = agent_context::cli_agent(&db, json)?;
    agent_context::authorize_cli_folders(
        &db,
        ctx.as_ref(),
        "snooze",
        &creds.account.id,
        &[folder],
        json,
    )?;

    // Check if already snoozed
    if let Some(existing) = db
        .find_snoozed_by_uid(account_email, uid)
        .context("database error")?
    {
        bail!(
            "UID {uid} is already snoozed (returns at {}). Unsnooze first.",
            existing.return_at
        );
    }

    // Connect to IMAP
    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;
    let folder = &imap::resolve_mailbox(&mut client, folder)
        .await
        .context("failed to resolve folder")?;

    agent_context::refuse_held_cli(
        &mut client,
        &db,
        ctx.as_ref(),
        "snooze",
        &creds.account.id,
        folder,
        uid,
        json,
    )
    .await?;

    // Ensure the Snoozed folder exists
    if let Err(e) = imap::create_folder(&mut client, IMAP_SNOOZED_FOLDER).await {
        warn!("could not create Snoozed folder (may already exist): {e}");
    }

    // Fetch message summary to store subject and message-id. A read the
    // server refused stops here, before the move.
    let msg = imap::fetch_message(&mut client, folder, uid)
        .await
        .with_context(|| {
            format!("could not read UID {uid} in {folder} on {account_email}; nothing was moved")
        })?;

    let (subject, message_id) = match &msg {
        Some(m) => (Some(m.subject.as_str()), m.message_id.as_deref()),
        None => (None, None),
    };

    // Move message to Snoozed folder
    imap::move_message(&mut client, uid, folder, IMAP_SNOOZED_FOLDER)
        .await
        .context("failed to move message to Snoozed folder")?;

    info!("moved UID {uid} from {folder} to {IMAP_SNOOZED_FOLDER}");

    // Record in local DB
    let snoozed = db
        .create_snoozed(
            account_email,
            uid,
            folder,
            IMAP_SNOOZED_FOLDER,
            &return_at,
            message_id,
            subject,
            reason,
            note,
            recipient,
        )
        .context("failed to record snooze in database")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&snoozed)?);
    } else {
        println!("Snoozed UID {uid}");
        println!("  From:     {folder}");
        println!("  Until:    {return_at}");
        if let Some(s) = subject {
            println!("  Subject:  {s}");
        }
        if let Some(r) = reason {
            println!("  Reason:   {r}");
        }
        if let Some(n) = note {
            println!("  Note:     {n}");
        }
        if let Some(r) = recipient {
            println!("  Waiting:  {r}");
        }
        println!("  ID:       {}", snoozed.id);
    }

    Ok(())
}

/// List all snoozed messages.
pub fn run_list(account: Option<&str>, json: bool, _backend: CredentialBackend) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    // Resolve account email if provided
    let account_filter = match account {
        Some(a) => {
            let acct = resolve_account(&db, Some(a))?;
            Some(acct.username)
        }
        None => None,
    };

    let snoozed = db
        .list_snoozed(account_filter.as_deref())
        .context("failed to list snoozed messages")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&snoozed)?);
    } else {
        if snoozed.is_empty() {
            println!("No snoozed messages");
            return Ok(());
        }

        println!(
            "{:<6}  {:<25}  {:<22}  {:<14}  {:<20}  {}",
            "UID", "ACCOUNT", "RETURN AT", "REASON", "SUBJECT", "NOTE"
        );
        println!("{}", "-".repeat(120));
        for s in &snoozed {
            let subject = s.subject.as_deref().unwrap_or("-");
            let subject_display = if subject.len() > 18 {
                format!("{}...", &subject[..15])
            } else {
                subject.to_string()
            };
            let account_display = if s.account.len() > 23 {
                format!("{}...", &s.account[..20])
            } else {
                s.account.clone()
            };
            let reason_display = s.reason.as_deref().unwrap_or("-");
            let note_display = match s.note.as_deref() {
                Some(n) if n.len() > 30 => format!("{}...", &n[..27]),
                Some(n) => n.to_string(),
                None => "-".to_string(),
            };
            println!(
                "{:<6}  {:<25}  {:<22}  {:<14}  {:<20}  {}",
                s.uid, account_display, s.return_at, reason_display, subject_display, note_display,
            );
        }
        println!("\n{} snoozed message(s)", snoozed.len());
    }

    Ok(())
}

/// Unsnooze a message: move it back to its original folder immediately. With
/// `forget`, delete the snooze instead and touch no mailbox, for a snooze
/// whose message is no longer in the snooze folder.
#[tokio::main]
pub async fn run_unsnooze(
    uid: u32,
    account: Option<&str>,
    forget: bool,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let account_email = &creds.account.username;
    let ctx = agent_context::cli_agent(&db, json)?;
    agent_context::authorize_cli_action(&db, ctx.as_ref(), "snooze", &creds.account.id, json)?;

    // Find the snoozed record
    let snoozed = db
        .find_snoozed_by_uid(account_email, uid)
        .context("database error")?
        .ok_or_else(|| {
            anyhow::anyhow!("no snoozed message found for UID {uid} on account {account_email}")
        })?;
    agent_context::authorize_cli_folders(
        &db,
        ctx.as_ref(),
        "snooze",
        &creds.account.id,
        &[&snoozed.original_folder],
        json,
    )?;

    if forget {
        db.delete_snoozed(&snoozed.id)
            .context("failed to delete the snooze")?;
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "action": "forget",
                    "uid": uid,
                    "moved": false,
                    "folder": snoozed.snoozed_folder,
                    "subject": snoozed.subject,
                })
            );
        } else {
            println!("Deleted the snooze of UID {uid}; nothing was moved");
            if let Some(ref s) = snoozed.subject {
                println!("  Subject: {s}");
            }
        }
        return Ok(());
    }

    // Connect to IMAP and move back
    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    cancel_snooze(&db, &mut client, &snoozed).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "unsnooze",
                "uid": uid,
                "from": snoozed.snoozed_folder,
                "to": snoozed.original_folder,
                "subject": snoozed.subject,
            })
        );
    } else {
        println!("Unsnoozed UID {uid}");
        println!("  Moved back to: {}", snoozed.original_folder);
        if let Some(ref s) = snoozed.subject {
            println!("  Subject: {s}");
        }
    }

    Ok(())
}

/// The mailbox calls a snoozed message's return makes: the live IMAP client,
/// or a fake in tests.
#[allow(async_fn_in_trait)]
pub(crate) trait SnoozeMailbox {
    async fn find_uid_by_message_id(
        &mut self,
        folder: &str,
        message_id: &str,
    ) -> Result<Option<u32>, ImapError>;
    async fn move_message(&mut self, uid: u32, from: &str, to: &str) -> Result<(), ImapError>;
}

impl SnoozeMailbox for imap::ImapClient {
    async fn find_uid_by_message_id(
        &mut self,
        folder: &str,
        message_id: &str,
    ) -> Result<Option<u32>, ImapError> {
        imap::find_uid_by_message_id(self, folder, message_id).await
    }

    async fn move_message(&mut self, uid: u32, from: &str, to: &str) -> Result<(), ImapError> {
        imap::move_message(self, uid, from, to).await
    }
}

/// Find `snoozed` in its snooze folder by its Message-ID. The stored UID
/// names the message in the folder it came from, so in the snooze folder it
/// may be another message, or none: it is never used. A snooze without a
/// Message-ID, a search that finds nothing, or a search that failed moves
/// nothing and keeps the snooze, and the error names the snooze and says why.
pub(crate) async fn find_in_snoozed(
    mailbox: &mut impl SnoozeMailbox,
    snoozed: &SnoozedMessage,
) -> Result<u32, String> {
    let folder = &snoozed.snoozed_folder;
    let account = &snoozed.account;
    let uid = snoozed.uid;
    let snooze = format!(
        "the snooze of UID {uid} from {} on {account}",
        snoozed.original_folder
    );
    let forget = format!("`envelope snooze cancel {uid} --account {account} --forget`");
    let Some(mid) = snoozed.message_id.as_deref() else {
        return Err(format!(
            "{snooze} has no Message-ID to find its message by in {folder}; nothing was moved. \
             Move the message out of {folder} by hand, then {forget} deletes the snooze"
        ));
    };
    mailbox
        .find_uid_by_message_id(folder, mid.trim_matches(|c| c == '<' || c == '>'))
        .await
        .map_err(|e| {
            format!("{snooze}: could not search {folder} for its message; nothing was moved: {e:#}")
        })?
        .ok_or_else(|| {
            format!(
                "{snooze}: its message {mid} is no longer in {folder}; nothing was moved. \
                 If it is gone, {forget} deletes the snooze without moving anything"
            )
        })
}

/// Move a snoozed message back to its original folder now, for `envelope
/// snooze cancel`, and clear its snooze. The message is found by
/// [`find_in_snoozed`].
async fn cancel_snooze(
    db: &Database,
    mailbox: &mut impl SnoozeMailbox,
    snoozed: &SnoozedMessage,
) -> Result<()> {
    let snoozed_uid = find_in_snoozed(mailbox, snoozed)
        .await
        .map_err(anyhow::Error::msg)?;

    mailbox
        .move_message(
            snoozed_uid,
            &snoozed.snoozed_folder,
            &snoozed.original_folder,
        )
        .await
        .context("failed to move message back to original folder")?;

    // Remove from local DB
    db.delete_snoozed(&snoozed.id)
        .context("failed to remove snooze record")?;
    Ok(())
}

/// Move one due snoozed message back to its original folder, for `envelope
/// unsnooze`, and clear its snooze. The message is found by
/// [`find_in_snoozed`].
async fn return_due(
    db: &Database,
    mailbox: &mut impl SnoozeMailbox,
    msg: &SnoozedMessage,
) -> Result<(), String> {
    let uid = find_in_snoozed(mailbox, msg).await?;
    mailbox
        .move_message(uid, &msg.snoozed_folder, &msg.original_folder)
        .await
        .map_err(|e| e.to_string())?;
    if let Err(e) = db.delete_snoozed(&msg.id) {
        warn!("failed to delete snooze record {}: {e}", msg.id);
    }
    Ok(())
}

/// Check all snoozed messages and unsnooze any that are past their return time.
/// JSON output includes reason/note/escalation_tier for agent consumption.
#[tokio::main]
pub async fn run_check(
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let now = Utc::now().format(RETURN_AT_FORMAT).to_string();
    let ctx = agent_context::cli_agent(&db, json)?;

    // Resolve account filter
    let account_filter = match account {
        Some(a) => {
            let acct = resolve_account(&db, Some(a))?;
            agent_context::authorize_cli_action(&db, ctx.as_ref(), "snooze", &acct.id, json)?;
            Some(acct.username.clone())
        }
        None => None,
    };

    let due = db
        .list_snoozed_due(&now, account_filter.as_deref())
        .context("failed to query due snoozed messages")?;

    if due.is_empty() {
        if json {
            println!("{}", serde_json::json!({ "unsnoozed": [] }));
        } else {
            println!("No snoozed messages due for return");
        }
        return Ok(());
    }

    // Group messages by account
    let mut by_account: std::collections::HashMap<String, Vec<_>> =
        std::collections::HashMap::new();
    for msg in &due {
        by_account.entry(msg.account.clone()).or_default().push(msg);
    }

    // An agent's sweep returns mail only within its policy: an account or
    // folder outside it refuses the whole sweep before any mailbox is touched.
    for (account_email, messages) in &by_account {
        let Some(acct) = db
            .find_account_by_email(account_email)
            .context("database error")?
        else {
            continue;
        };
        for msg in messages {
            agent_context::authorize_cli_folders(
                &db,
                ctx.as_ref(),
                "snooze",
                &acct.id,
                &[&msg.original_folder],
                json,
            )?;
        }
    }

    // We need credentials per account to connect to IMAP.
    let passphrase = envelope_email_store::credential_store::get_or_create_passphrase(backend)
        .context("credential store error")?;

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut success_count = 0u32;
    let mut fail_count = 0u32;

    for (account_email, messages) in &by_account {
        // Resolve account and get credentials
        let acct = match db
            .find_account_by_email(account_email)
            .context("database error")?
        {
            Some(a) => a,
            None => {
                warn!("account not found for snoozed messages: {account_email}");
                fail_count += messages.len() as u32;
                continue;
            }
        };

        let creds = match db.get_account_with_credentials(&acct.id, &passphrase) {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to decrypt credentials for {account_email}: {e}");
                fail_count += messages.len() as u32;
                continue;
            }
        };

        let mut client = match imap::connect(&creds).await {
            Ok(c) => c,
            Err(e) => {
                warn!("IMAP connection failed for {account_email}: {e}");
                fail_count += messages.len() as u32;
                continue;
            }
        };

        for msg in messages {
            let moved = return_due(&db, &mut client, msg).await;

            match moved {
                Ok(()) => {
                    success_count += 1;
                    info!("unsnoozed UID {} -> {}", msg.uid, msg.original_folder);

                    if !json {
                        let subj = msg.subject.as_deref().unwrap_or("-");
                        println!("  ✓ UID {} → {} ({})", msg.uid, msg.original_folder, subj);
                    }

                    results.push(serde_json::json!({
                        "uid": msg.uid,
                        "account": msg.account,
                        "to": msg.original_folder,
                        "subject": msg.subject,
                        "reason": msg.reason,
                        "note": msg.note,
                        "recipient": msg.recipient,
                        "escalation_tier": msg.escalation_tier,
                        "reply_received": msg.reply_received,
                        "status": "ok",
                    }));
                }
                Err(e) => {
                    warn!("failed to unsnooze UID {}: {e}", msg.uid);
                    fail_count += 1;

                    if !json {
                        println!("  ✗ UID {} — {e}", msg.uid);
                    }

                    results.push(serde_json::json!({
                        "uid": msg.uid,
                        "account": msg.account,
                        "error": format!("{e}"),
                        "reason": msg.reason,
                        "note": msg.note,
                        "escalation_tier": msg.escalation_tier,
                        "status": "error",
                    }));
                }
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "unsnoozed": results,
                "success": success_count,
                "failed": fail_count,
            }))?
        );
    } else {
        println!("\n{success_count} unsnoozed, {fail_count} failed");
    }

    Ok(())
}

/// Check for replies to snoozed messages that are waiting for a response.
///
/// For each snoozed message with reason "waiting-reply" or "follow-up":
/// 1. Extract the recipient email from the snoozed record
/// 2. Search ALL configured accounts' inboxes for recent messages FROM that recipient
/// 3. If a reply is found: mark reply_received = 1, output the match
/// 4. If no reply and past the --until time: increment escalation_tier
/// 5. JSON output for agent consumption
#[tokio::main]
pub async fn run_check_replies(
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let ctx = agent_context::cli_agent(&db, json)?;

    // Resolve account filter
    let account_filter = match account {
        Some(a) => {
            let acct = resolve_account(&db, Some(a))?;
            agent_context::authorize_cli_action(&db, ctx.as_ref(), "snooze", &acct.id, json)?;
            Some(acct.username.clone())
        }
        None => None,
    };

    // Get all snoozed messages awaiting reply
    let awaiting = db
        .list_snoozed_awaiting_reply(account_filter.as_deref())
        .context("failed to query snoozed messages awaiting reply")?;

    if awaiting.is_empty() {
        if json {
            println!("{}", serde_json::json!({ "results": [], "checked": 0 }));
        } else {
            println!("No snoozed messages awaiting replies");
        }
        return Ok(());
    }

    // Collect unique recipient emails to search for
    let recipients_to_check: Vec<(String, Vec<&envelope_email_store::SnoozedMessage>)> = {
        let mut by_recipient: std::collections::HashMap<
            String,
            Vec<&envelope_email_store::SnoozedMessage>,
        > = std::collections::HashMap::new();
        for msg in &awaiting {
            if let Some(ref recip) = msg.recipient {
                by_recipient.entry(recip.clone()).or_default().push(msg);
            }
        }
        by_recipient.into_iter().collect()
    };

    // Get all configured accounts for cross-account search
    let all_accounts = db.list_accounts().context("failed to list accounts")?;
    // The search reads every account's INBOX, so an agent needs them all: a
    // search limited to its own accounts would escalate snoozes whose reply
    // sits in one it cannot see.
    for acct in &all_accounts {
        agent_context::authorize_cli_folders(
            &db,
            ctx.as_ref(),
            "snooze",
            &acct.id,
            &["INBOX"],
            json,
        )?;
    }
    let passphrase = envelope_email_store::credential_store::get_or_create_passphrase(backend)
        .context("credential store error")?;

    // For each account, connect and search for messages FROM each recipient
    let mut reply_found_for: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut reply_details: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    // Recipients whose reply was not looked for in some mailbox, and why. A
    // reply may be there, so they are not escalated.
    let mut unchecked: HashMap<String, String> = HashMap::new();
    let not_searched = |unchecked: &mut HashMap<String, String>, why: String| {
        for (recipient, _) in &recipients_to_check {
            unchecked
                .entry(recipient.clone())
                .or_insert_with(|| why.clone());
        }
    };

    for acct in &all_accounts {
        let creds = match db.get_account_with_credentials(&acct.id, &passphrase) {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to decrypt credentials for {}: {e}", acct.username);
                not_searched(&mut unchecked, format!("{}: {e}", acct.username));
                continue;
            }
        };

        let mut client = match imap::connect(&creds).await {
            Ok(c) => c,
            Err(e) => {
                warn!("IMAP connection failed for {}: {e}", acct.username);
                not_searched(&mut unchecked, format!("{}: {e}", acct.username));
                continue;
            }
        };

        for (recipient, _msgs) in &recipients_to_check {
            if reply_found_for.contains(recipient) {
                continue; // Already found a reply for this recipient
            }

            // IMAP search: FROM <recipient> in INBOX, last 30 days
            let search_query = format!("FROM \"{}\" SINCE 1-Jan-2026", recipient);
            match imap::search(&mut client, "INBOX", &search_query, 5).await {
                Ok(results) if !results.is_empty() => {
                    let newest = &results[0];
                    reply_found_for.insert(recipient.clone());
                    reply_details.insert(
                        recipient.clone(),
                        serde_json::json!({
                            "found_in_account": acct.username,
                            "from": newest.from_addr,
                            "subject": newest.subject,
                            "date": newest.date,
                            "uid": newest.uid,
                        }),
                    );
                    info!(
                        "found reply from {recipient} in {} (UID {})",
                        acct.username, newest.uid
                    );
                }
                Ok(_) => {} // No results
                Err(e) => {
                    warn!(
                        "search failed for FROM {} in {}: {e}",
                        recipient, acct.username
                    );
                    unchecked
                        .entry(recipient.clone())
                        .or_insert_with(|| format!("INBOX on {}: {e}", acct.username));
                }
            }
        }
    }

    // Now process results: update DB and build output
    let results = settle_awaiting(&db, &awaiting, &reply_details, &unchecked, Utc::now(), json);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "results": results,
                "checked": awaiting.len(),
            }))?
        );
    } else {
        println!(
            "\nChecked {} snoozed message(s) awaiting replies",
            awaiting.len()
        );
    }

    Ok(())
}

/// Record what one `check-replies` found for each snoozed message awaiting a
/// reply, and build its result rows. `reply_details` holds the replies found,
/// by recipient; `unchecked` the recipients whose reply could not be looked
/// for in some mailbox, with why. A message with no reply found is escalated
/// once it is overdue: its naive UTC `return_at` is at or before `now`.
fn settle_awaiting(
    db: &Database,
    awaiting: &[SnoozedMessage],
    reply_details: &HashMap<String, serde_json::Value>,
    unchecked: &HashMap<String, String>,
    now: DateTime<Utc>,
    json: bool,
) -> Vec<serde_json::Value> {
    let now = now.format(RETURN_AT_FORMAT).to_string();
    let mut results: Vec<serde_json::Value> = Vec::new();
    for msg in awaiting {
        let recipient = msg.recipient.as_deref().unwrap_or("(unknown)");

        if let Some(reply_info) = msg.recipient.as_ref().and_then(|r| reply_details.get(r)) {
            // Reply found — mark in DB
            if let Err(e) = db.mark_reply_received(&msg.id) {
                warn!("failed to mark reply_received for {}: {e}", msg.id);
            }

            if !json {
                println!(
                    "  ✓ UID {} — reply from {} found! {}",
                    msg.uid,
                    recipient,
                    reply_info
                        .get("subject")
                        .and_then(|s| s.as_str())
                        .unwrap_or("-")
                );
            }

            results.push(serde_json::json!({
                "uid": msg.uid,
                "account": msg.account,
                "subject": msg.subject,
                "reason": msg.reason,
                "note": msg.note,
                "recipient": recipient,
                "status": "reply_found",
                "reply": reply_info,
                "escalation_tier": msg.escalation_tier,
            }));
        } else if msg.recipient.is_none() {
            // No recipient recorded — can't check
            if !json {
                println!(
                    "  ? UID {} — no recipient recorded, cannot check replies",
                    msg.uid
                );
            }
            results.push(serde_json::json!({
                "uid": msg.uid,
                "account": msg.account,
                "subject": msg.subject,
                "reason": msg.reason,
                "note": msg.note,
                "recipient": null,
                "status": "no_recipient",
                "escalation_tier": msg.escalation_tier,
            }));
        } else if let Some(why) = msg.recipient.as_ref().and_then(|r| unchecked.get(r)) {
            // No reply found, but a mailbox it could be in was not searched:
            // nothing is escalated, and the next check looks again.
            if !json {
                println!(
                    "  ! UID {} — could not check for a reply from {recipient}: {why}",
                    msg.uid
                );
            }
            results.push(serde_json::json!({
                "uid": msg.uid,
                "account": msg.account,
                "subject": msg.subject,
                "reason": msg.reason,
                "note": msg.note,
                "recipient": recipient,
                "status": "check_failed",
                "error": why,
                "escalation_tier": msg.escalation_tier,
            }));
        } else {
            // No reply found
            let is_overdue = msg.return_at <= now;
            let new_tier = if is_overdue {
                // Past due — escalate
                match db.increment_escalation(&msg.id) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("failed to increment escalation for {}: {e}", msg.id);
                        msg.escalation_tier
                    }
                }
            } else {
                msg.escalation_tier
            };

            if !json {
                if is_overdue {
                    println!(
                        "  ✗ UID {} — no reply from {} (overdue, escalation tier {})",
                        msg.uid, recipient, new_tier
                    );
                } else {
                    println!(
                        "  … UID {} — no reply from {} yet (due {})",
                        msg.uid, recipient, msg.return_at
                    );
                }
            }

            results.push(serde_json::json!({
                "uid": msg.uid,
                "account": msg.account,
                "subject": msg.subject,
                "reason": msg.reason,
                "note": msg.note,
                "recipient": recipient,
                "status": if is_overdue { "no_reply_overdue" } else { "no_reply_waiting" },
                "escalation_tier": new_tier,
                "overdue": is_overdue,
                "return_at": msg.return_at,
            }));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn validate_reason_valid() {
        assert!(validate_reason("follow-up").is_ok());
        assert!(validate_reason("waiting-reply").is_ok());
        assert!(validate_reason("defer").is_ok());
        assert!(validate_reason("reminder").is_ok());
        assert!(validate_reason("review").is_ok());
    }

    #[test]
    fn validate_reason_invalid() {
        assert!(validate_reason("banana").is_err());
        assert!(validate_reason("").is_err());
    }

    const MID: &str = "quote-1@corp.test";

    /// Another snoozed message, which sits in Snoozed at the UID the snooze
    /// under test held in INBOX.
    const OTHER: &str = "other-snooze@corp.test";

    /// A Snoozed folder held in memory: UID to Message-ID. It counts each
    /// search and records each move it was asked for.
    #[derive(Default)]
    struct FakeSnoozed {
        held: std::collections::BTreeMap<u32, String>,
        refuse_search: bool,
        searches: usize,
        moves: Vec<u32>,
        returned: Vec<String>,
    }

    impl FakeSnoozed {
        fn holding(held: &[(u32, &str)]) -> Self {
            Self {
                held: held.iter().map(|(u, m)| (*u, m.to_string())).collect(),
                ..Self::default()
            }
        }
    }

    impl SnoozeMailbox for FakeSnoozed {
        async fn find_uid_by_message_id(
            &mut self,
            folder: &str,
            message_id: &str,
        ) -> Result<Option<u32>, ImapError> {
            assert_eq!(folder, "Snoozed");
            self.searches += 1;
            if self.refuse_search {
                return Err(ImapError::Protocol(format!(
                    "UID SEARCH HEADER Message-ID \"{message_id}\": the server refused it: \
                     NO [UNAVAILABLE] backend down"
                )));
            }
            Ok(self
                .held
                .iter()
                .find(|(_, m)| m.as_str() == message_id)
                .map(|(u, _)| *u))
        }

        async fn move_message(&mut self, uid: u32, from: &str, to: &str) -> Result<(), ImapError> {
            assert_eq!((from, to), ("Snoozed", "INBOX"));
            self.moves.push(uid);
            if let Some(mid) = self.held.remove(&uid) {
                self.returned.push(mid);
            }
            Ok(())
        }
    }

    /// A snooze of the message that was UID 42 in INBOX on `me@corp.test`,
    /// with or without its Message-ID.
    fn snoozed_from_inbox(message_id: Option<&str>) -> (Database, SnoozedMessage) {
        let db = Database::open_memory().unwrap();
        let snooze = db
            .create_snoozed(
                "me@corp.test",
                42,
                "INBOX",
                "Snoozed",
                "2031-02-03T04:00:00",
                message_id.map(|m| format!("<{m}>")).as_deref(),
                Some("Quote"),
                Some("follow-up"),
                None,
                None,
            )
            .unwrap();
        (db, snooze)
    }

    /// The two CLI returns: `envelope unsnooze`, which returns due snoozes,
    /// and `envelope snooze cancel`, which returns one by hand.
    #[derive(Clone, Copy, Debug)]
    enum CliReturn {
        Unsnooze,
        Cancel,
    }

    impl CliReturn {
        const BOTH: [CliReturn; 2] = [CliReturn::Unsnooze, CliReturn::Cancel];

        /// Run it; the error as the command shows it.
        async fn run(
            self,
            db: &Database,
            mailbox: &mut FakeSnoozed,
            snooze: &SnoozedMessage,
        ) -> Result<(), String> {
            match self {
                CliReturn::Unsnooze => return_due(db, mailbox, snooze).await,
                CliReturn::Cancel => cancel_snooze(db, mailbox, snooze)
                    .await
                    .map_err(|e| format!("{e:#}")),
            }
        }
    }

    /// A CLI return that moved nothing: the error names the snooze (its UID,
    /// the folder it came from, the account) and says nothing was moved, no
    /// move was asked for, the other message stays at UID 42 in Snoozed, and
    /// the snooze is kept.
    fn assert_cli_return_moved_nothing(
        how: CliReturn,
        err: &str,
        db: &Database,
        mailbox: &FakeSnoozed,
        snooze: &SnoozedMessage,
    ) {
        for expected in [
            "UID 42",
            "INBOX",
            "Snoozed",
            "me@corp.test",
            "nothing was moved",
        ] {
            assert!(
                err.contains(expected),
                "{how:?}: missing {expected:?}: {err}"
            );
        }
        assert!(
            mailbox.moves.is_empty(),
            "{how:?}: moved {:?}",
            mailbox.moves
        );
        assert_eq!(
            mailbox.held.get(&42).map(String::as_str),
            Some(OTHER),
            "{how:?}"
        );
        assert!(
            db.get_snoozed(&snooze.id).unwrap().is_some(),
            "{how:?}: the snooze is kept"
        );
    }

    /// The snoozed message is no longer in Snoozed, and UID 42 there, the UID
    /// it held in INBOX, is another snoozed message. Neither CLI return moves
    /// it: each errors, keeps the snooze, and says how to drop it.
    #[tokio::test]
    async fn cli_returns_never_move_a_uid_that_collides_in_snoozed() {
        for how in CliReturn::BOTH {
            let (db, snooze) = snoozed_from_inbox(Some(MID));
            let mut mailbox = FakeSnoozed::holding(&[(42, OTHER)]);

            let err = how
                .run(&db, &mut mailbox, &snooze)
                .await
                .expect_err("a message that is gone is not returned");

            assert_cli_return_moved_nothing(how, &err, &db, &mailbox, &snooze);
            assert!(err.contains(MID), "{how:?}: {err}");
            assert!(
                err.contains("envelope snooze cancel 42 --account me@corp.test --forget"),
                "{how:?}: {err}"
            );
        }
    }

    /// No message in Snoozed carries the Message-ID, and nothing holds UID
    /// 42 there either: no move is asked for, and the snooze stays.
    #[tokio::test]
    async fn cli_returns_move_nothing_when_the_message_is_gone() {
        for how in CliReturn::BOTH {
            let (db, snooze) = snoozed_from_inbox(Some(MID));
            let mut mailbox = FakeSnoozed::default();

            let err = how
                .run(&db, &mut mailbox, &snooze)
                .await
                .expect_err("a message that is gone is not returned");

            assert!(
                mailbox.moves.is_empty(),
                "{how:?}: moved {:?}",
                mailbox.moves
            );
            assert!(db.get_snoozed(&snooze.id).unwrap().is_some(), "{how:?}");
            assert!(err.contains("nothing was moved"), "{how:?}: {err}");
        }
    }

    /// A snooze recorded without a Message-ID cannot be found in Snoozed:
    /// neither CLI return guesses with the INBOX UID, which in Snoozed is
    /// another message.
    #[tokio::test]
    async fn cli_returns_move_nothing_without_a_message_id() {
        for how in CliReturn::BOTH {
            let (db, snooze) = snoozed_from_inbox(None);
            let mut mailbox = FakeSnoozed::holding(&[(42, OTHER)]);

            let err = how
                .run(&db, &mut mailbox, &snooze)
                .await
                .expect_err("nothing to find it by");

            assert_eq!(mailbox.searches, 0, "{how:?}: nothing to search for");
            assert_cli_return_moved_nothing(how, &err, &db, &mailbox, &snooze);
            assert!(err.contains("no Message-ID"), "{how:?}: {err}");
            assert!(err.contains("--forget"), "{how:?}: {err}");
        }
    }

    /// A search the server refused says nothing about where the message is:
    /// each CLI return errors with the server's reply and moves nothing.
    #[tokio::test]
    async fn cli_returns_error_when_the_search_is_refused() {
        for how in CliReturn::BOTH {
            let (db, snooze) = snoozed_from_inbox(Some(MID));
            let mut mailbox = FakeSnoozed::holding(&[(5, MID), (42, OTHER)]);
            mailbox.refuse_search = true;

            let err = how
                .run(&db, &mut mailbox, &snooze)
                .await
                .expect_err("a refused search returns nothing");

            assert_cli_return_moved_nothing(how, &err, &db, &mailbox, &snooze);
            assert!(
                err.contains("NO [UNAVAILABLE] backend down"),
                "{how:?}: {err}"
            );
            assert_eq!(
                mailbox.held.get(&5).map(String::as_str),
                Some(MID),
                "{how:?}"
            );
        }
    }

    /// With the message still in Snoozed at UID 5 and another message at UID
    /// 42, each CLI return moves the message it found, the other stays, and
    /// the snooze is cleared.
    #[tokio::test]
    async fn cli_returns_move_the_message_found_past_a_colliding_uid() {
        for how in CliReturn::BOTH {
            let (db, snooze) = snoozed_from_inbox(Some(MID));
            let mut mailbox = FakeSnoozed::holding(&[(5, MID), (42, OTHER)]);

            how.run(&db, &mut mailbox, &snooze).await.unwrap();

            assert_eq!(mailbox.moves, [5], "{how:?}");
            assert_eq!(mailbox.returned, [MID], "{how:?}");
            assert_eq!(
                mailbox.held.get(&42).map(String::as_str),
                Some(OTHER),
                "{how:?}"
            );
            assert!(db.get_snoozed(&snooze.id).unwrap().is_none(), "{how:?}");
        }
    }

    /// A reply search the server refused, or a mailbox that could not be
    /// opened, found no reply because it never looked. The follow-up is
    /// reported as not checked, with the reason, and is not escalated; the
    /// next check looks again.
    #[test]
    fn a_follow_up_whose_reply_could_not_be_looked_for_is_not_escalated() {
        let db = Database::open_memory().unwrap();
        db.create_snoozed(
            "me@example.test",
            7,
            "INBOX",
            "Snoozed",
            "2026-01-01T09:00:00",
            Some("<sent-1@example.test>"),
            Some("Quote"),
            Some("waiting-reply"),
            None,
            Some("them@example.test"),
        )
        .unwrap();
        let awaiting = db.list_snoozed_awaiting_reply(None).unwrap();
        let why = "me@example.test: UID SEARCH FROM \"them@example.test\" SINCE 1-Jan-2026: \
                   the server refused it: NO [UNAVAILABLE] backend down";
        let unchecked = HashMap::from([("them@example.test".to_string(), why.to_string())]);

        let results = settle_awaiting(
            &db,
            &awaiting,
            &HashMap::new(),
            &unchecked,
            utc("2026-10-12T09:00:00Z"),
            true,
        );

        assert_eq!(results[0]["status"], "check_failed", "{results:?}");
        assert!(
            results[0]["error"]
                .as_str()
                .is_some_and(|e| e.contains("NO [UNAVAILABLE] backend down")),
            "{results:?}"
        );
        let row = db.get_snoozed(&awaiting[0].id).unwrap().unwrap();
        assert_eq!(row.escalation_tier, 0, "nothing is escalated");
    }

    /// A follow-up with no reply is overdue from its naive UTC `return_at`
    /// on, and not a second before.
    #[test]
    fn a_follow_up_is_overdue_from_its_utc_return_time() {
        let db = Database::open_memory().unwrap();
        db.create_snoozed(
            "me@example.test",
            7,
            "INBOX",
            "Snoozed",
            "2026-10-12T09:00:00",
            Some("<sent-1@example.test>"),
            Some("Quote"),
            Some("waiting-reply"),
            None,
            Some("them@example.test"),
        )
        .unwrap();
        let awaiting = db.list_snoozed_awaiting_reply(None).unwrap();
        let no_replies = HashMap::new();
        let all_searched = HashMap::new();
        let settle = |now| settle_awaiting(&db, &awaiting, &no_replies, &all_searched, now, true);

        let before = settle(utc("2026-10-12T08:59:59Z"));
        assert_eq!(before[0]["status"], "no_reply_waiting", "{before:?}");
        assert_eq!(before[0]["escalation_tier"], 0, "{before:?}");

        let at = settle(utc("2026-10-12T09:00:00Z"));
        assert_eq!(at[0]["status"], "no_reply_overdue", "{at:?}");
        assert_eq!(at[0]["escalation_tier"], 1, "{at:?}");
    }
}
