// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::Database;
use envelope_email_store::credential_store::{self, CredentialBackend};
use envelope_email_transport::SmtpSender;
use envelope_email_transport::imap;
use envelope_email_transport::{detect_drafts_folder, detect_sent_folder};
use mail_builder::MessageBuilder;
use tracing::warn;

use super::common::{resolve_account, setup_credentials};

/// Build an RFC822-formatted draft message suitable for IMAP APPEND.
///
/// Returns (rfc822_bytes, message_id).
fn build_rfc822_draft(
    from: &str,
    to: &str,
    subject: Option<&str>,
    body: Option<&str>,
    cc: Option<&str>,
    in_reply_to: Option<&str>,
) -> Result<(Vec<u8>, String)> {
    let mut builder = MessageBuilder::new()
        .from(from)
        .to(to)
        .subject(subject.unwrap_or(""));

    if let Some(cc_addr) = cc {
        builder = builder.cc(cc_addr);
    }

    if let Some(irt) = in_reply_to {
        builder = builder.in_reply_to(irt);
    }

    let text = body.unwrap_or("");
    builder = builder.text_body(text);

    let rfc822 = builder
        .write_to_string()
        .context("failed to build RFC822 message")?;

    // Extract the Message-ID from the generated RFC822
    let message_id = rfc822
        .lines()
        .find(|l| l.to_lowercase().starts_with("message-id:"))
        .map(|l| {
            l.split_once(':')
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    Ok((rfc822.into_bytes(), message_id))
}

// ─── draft list ──────────────────────────────────────────────────────────

#[tokio::main]
pub async fn run_list(account: Option<&str>, json: bool, backend: CredentialBackend) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let passphrase =
        credential_store::get_or_create_passphrase(backend).context("credential store error")?;
    let acct = resolve_account(&db, account)?;

    // Check if account has IMAP
    if acct.imap_host.is_empty() {
        // Send-only account: fall back to local SQLite
        return run_list_local(&db, &acct.id, json);
    }

    let creds = db
        .get_account_with_credentials(&acct.id, &passphrase)
        .context("failed to decrypt credentials")?;

    // Try IMAP first — that's the source of truth
    match imap::connect(&creds).await {
        Ok(mut client) => {
            let drafts_folder = detect_drafts_folder(&mut client, &db, &acct.id)
                .await
                .map_err(|e| anyhow::anyhow!("drafts folder detection failed: {e}"))?;
            let drafts_folder = match drafts_folder {
                Some(f) => f,
                None => {
                    warn!(
                        "no drafts folder detected for {}, falling back to local",
                        acct.username
                    );
                    return run_list_local(&db, &acct.id, json);
                }
            };

            // Fetch all messages from the Drafts folder
            let summaries = imap::fetch_inbox(&mut client, &drafts_folder, 100)
                .await
                .map_err(|e| anyhow::anyhow!("failed to fetch drafts from IMAP: {e}"))?;

            if json {
                let items: Vec<serde_json::Value> = summaries
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "uid": s.uid,
                            "from": s.from_addr,
                            "to": s.to_addr,
                            "subject": s.subject,
                            "date": s.date,
                            "size": s.size,
                            "message_id": s.message_id,
                            "flags": s.flags,
                            "source": "imap",
                            "folder": drafts_folder,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                if summaries.is_empty() {
                    println!("No drafts for {} (IMAP: {})", acct.username, drafts_folder);
                    return Ok(());
                }

                println!("{:<8}  {:<30}  {:<40}  {}", "UID", "TO", "SUBJECT", "DATE");
                println!("{}", "-".repeat(90));
                for s in &summaries {
                    let subject_display = if s.subject.len() > 38 {
                        format!("{}...", &s.subject[..38])
                    } else {
                        s.subject.clone()
                    };
                    let to_display = if s.to_addr.len() > 28 {
                        format!("{}...", &s.to_addr[..28])
                    } else {
                        s.to_addr.clone()
                    };
                    let date_display = s.date.as_deref().unwrap_or("-");
                    println!(
                        "{:<8}  {:<30}  {:<40}  {}",
                        s.uid, to_display, subject_display, date_display,
                    );
                }
                println!("\n{} draft(s) in {} (IMAP)", summaries.len(), drafts_folder);
            }
            Ok(())
        }
        Err(e) => {
            warn!("IMAP connect failed, falling back to local: {e}");
            run_list_local(&db, &acct.id, json)
        }
    }
}

/// Fallback: list drafts from local SQLite when IMAP is unavailable.
fn run_list_local(db: &Database, account_id: &str, json: bool) -> Result<()> {
    let drafts = db
        .list_drafts(account_id, Some("draft"), 100, 0)
        .context("failed to list drafts")?;

    if json {
        let items: Vec<serde_json::Value> = drafts
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "to": d.to_addr,
                    "subject": d.subject,
                    "updated_at": d.updated_at,
                    "imap_uid": d.imap_uid,
                    "source": "local",
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        if drafts.is_empty() {
            println!("No local drafts");
            return Ok(());
        }

        println!(
            "{:<36}  {:<30}  {:<40}  {}",
            "ID", "TO", "SUBJECT", "UPDATED"
        );
        println!("{}", "-".repeat(110));
        for d in &drafts {
            let subject = d.subject.as_deref().unwrap_or("-");
            let subject_display = if subject.len() > 38 {
                format!("{}...", &subject[..38])
            } else {
                subject.to_string()
            };
            let to_display = if d.to_addr.len() > 28 {
                format!("{}...", &d.to_addr[..28])
            } else {
                d.to_addr.clone()
            };
            println!(
                "{:<36}  {:<30}  {:<40}  {}",
                d.id, to_display, subject_display, d.updated_at,
            );
        }
        println!(
            "\n{} draft(s) (local only — IMAP unavailable)",
            drafts.len()
        );
    }

    Ok(())
}

// ─── draft create ────────────────────────────────────────────────────────

#[tokio::main]
pub async fn run_create(
    to: &str,
    subject: Option<&str>,
    body: Option<&str>,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
    from: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    in_reply_to: Option<&str>,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;

    // Build RFC822 message for IMAP APPEND
    let from_addr = if let Some(f) = from {
        f.to_string()
    } else if let Some(ref display) = creds.account.display_name {
        format!("{display} <{}>", creds.account.username)
    } else {
        creds.account.username.clone()
    };

    let (rfc822, message_id) = build_rfc822_draft(&from_addr, to, subject, body, cc, in_reply_to)?;

    // Check if this is a send-only account (no IMAP)
    let has_imap = !creds.account.imap_host.is_empty();

    let mut imap_uid: Option<u32> = None;
    let mut imap_synced = false;
    let mut drafts_folder_name = String::from("Drafts");

    if has_imap {
        // ── IMAP-first: APPEND to the Drafts folder ──
        match imap::connect(&creds).await {
            Ok(mut client) => {
                // Detect the correct Drafts folder for this account
                let detected = detect_drafts_folder(&mut client, &db, &creds.account.id).await;
                match detected {
                    Ok(Some(folder)) => {
                        drafts_folder_name = folder.clone();
                        match imap::append_message(
                            &mut client,
                            &folder,
                            "(\\Draft \\Seen)",
                            &rfc822,
                        )
                        .await
                        {
                            Ok(()) => {
                                imap_synced = true;
                                // Try to find the UID of the appended message
                                if !message_id.is_empty() {
                                    let mid_clean =
                                        message_id.trim_matches(|c| c == '<' || c == '>');
                                    match imap::find_uid_by_message_id(
                                        &mut client,
                                        &folder,
                                        mid_clean,
                                    )
                                    .await
                                    {
                                        Ok(Some(uid)) => imap_uid = Some(uid),
                                        Ok(None) => warn!(
                                            "IMAP APPEND succeeded but could not find UID by Message-ID"
                                        ),
                                        Err(e) => {
                                            warn!("failed to search for appended draft UID: {e}")
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("IMAP APPEND to {folder} failed: {e}");
                            }
                        }
                    }
                    Ok(None) => {
                        warn!(
                            "no drafts folder detected for {}; saving locally only",
                            creds.account.username
                        );
                    }
                    Err(e) => {
                        warn!("drafts folder detection failed: {e}; saving locally only");
                    }
                }
            }
            Err(e) => {
                warn!("IMAP connect failed: {e}; saving draft locally only");
            }
        }
    } else {
        warn!(
            "account {} has no IMAP — draft saved locally only (send-only account)",
            creds.account.username
        );
    }

    // ── Local SQLite record: secondary cache/reference ──
    let draft = db
        .create_draft(
            &creds.account.id,
            to,
            subject,
            body,
            None, // html_content
            in_reply_to,
            cc,
            bcc,
            Some("cli"),
        )
        .context("failed to create local draft record")?;

    // Store the IMAP UID in the local DB if we got one
    if let Some(uid) = imap_uid {
        if let Err(e) = db.update_draft_imap_uid(&draft.id, uid) {
            warn!("failed to store IMAP UID in local DB: {e}");
        }
    }

    // Store the message_id in local DB
    if !message_id.is_empty() {
        let _ = db.mark_draft_message_id(&draft.id, &message_id);
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": draft.id,
                "to": draft.to_addr,
                "subject": draft.subject,
                "cc": cc,
                "bcc": bcc,
                "in_reply_to": in_reply_to,
                "imap_synced": imap_synced,
                "imap_uid": imap_uid,
                "imap_folder": if imap_synced { Some(&drafts_folder_name) } else { None },
                "local_only": !imap_synced,
                "warning": if !imap_synced && has_imap {
                    Some("IMAP sync failed — draft saved locally only. Retry with draft create or check IMAP connectivity.")
                } else if !has_imap {
                    Some("Send-only account (no IMAP) — draft is local only.")
                } else {
                    None
                },
            })
        );
    } else {
        println!("Draft created: {}", draft.id);
        println!("  To:      {}", draft.to_addr);
        if let Some(ref s) = draft.subject {
            println!("  Subject: {s}");
        }
        if let Some(c) = cc {
            println!("  CC:      {c}");
        }
        if imap_synced {
            if let Some(uid) = imap_uid {
                println!("  IMAP:    synced to {} (UID {})", drafts_folder_name, uid);
            } else {
                println!("  IMAP:    synced to {} (UID pending)", drafts_folder_name);
            }
        } else if has_imap {
            println!("  ⚠ IMAP:  sync failed — saved locally only");
        } else {
            println!("  ⚠ IMAP:  send-only account — saved locally only");
        }
    }

    Ok(())
}

// ─── draft send ──────────────────────────────────────────────────────────

#[tokio::main]
pub async fn run_send(
    id: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let passphrase =
        credential_store::get_or_create_passphrase(backend).context("credential store error")?;

    // `id` can be either a local draft UUID or an IMAP UID (numeric).
    let is_imap_uid = id.parse::<u32>().is_ok();
    let local_draft = db.get_draft(id).context("failed to get draft")?;

    // Resolve account
    let acct = match account {
        Some(a) => resolve_account(&db, Some(a))?,
        None => {
            if let Some(ref d) = local_draft {
                db.get_account(&d.account_id)
                    .context("database error")?
                    .ok_or_else(|| {
                        anyhow::anyhow!("account not found for draft: {}", d.account_id)
                    })?
            } else {
                let acct = db
                    .default_account()
                    .context("failed to query default account")?;
                acct.ok_or_else(|| {
                    anyhow::anyhow!(
                        "no --account specified and no default account. \
                         Use --account to specify which account this IMAP draft belongs to."
                    )
                })?
            }
        }
    };

    let creds = db
        .get_account_with_credentials(&acct.id, &passphrase)
        .context("failed to decrypt credentials")?;

    // Determine the IMAP UID to fetch the draft from
    let imap_uid: Option<u32> = if let Some(ref d) = local_draft {
        d.imap_uid
    } else if is_imap_uid {
        Some(id.parse::<u32>().unwrap())
    } else {
        None
    };

    // ── Fetch draft content from IMAP (source of truth) ──
    let (to_addr, subject, text_body, html_body, cc_addr, bcc_addr, reply_to) =
        if let Some(uid) = imap_uid {
            if acct.imap_host.is_empty() {
                if let Some(ref d) = local_draft {
                    (
                        d.to_addr.clone(),
                        d.subject.clone().unwrap_or_default(),
                        d.text_content.clone(),
                        d.html_content.clone(),
                        d.cc_addr.clone(),
                        d.bcc_addr.clone(),
                        d.reply_to.clone(),
                    )
                } else {
                    bail!("draft {id} not found locally and account has no IMAP");
                }
            } else {
                let mut client = imap::connect(&creds)
                    .await
                    .context("failed to connect to IMAP to fetch draft")?;

                let drafts_folder = detect_drafts_folder(&mut client, &db, &acct.id)
                    .await
                    .map_err(|e| anyhow::anyhow!("drafts folder detection failed: {e}"))?
                    .unwrap_or_else(|| "Drafts".to_string());

                let msg = imap::fetch_message(&mut client, &drafts_folder, uid)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to fetch draft UID {uid} from IMAP: {e}"))?
                    .ok_or_else(|| {
                        anyhow::anyhow!("draft UID {uid} not found in IMAP {drafts_folder}")
                    })?;

                (
                    msg.to_addr,
                    msg.subject,
                    msg.text_body,
                    msg.html_body,
                    msg.cc_addr,
                    None::<String>,
                    None::<String>,
                )
            }
        } else if let Some(ref d) = local_draft {
            (
                d.to_addr.clone(),
                d.subject.clone().unwrap_or_default(),
                d.text_content.clone(),
                d.html_content.clone(),
                d.cc_addr.clone(),
                d.bcc_addr.clone(),
                d.reply_to.clone(),
            )
        } else {
            bail!("draft not found: {id}");
        };

    // ── Send via SMTP ──
    let message_id = SmtpSender::send_simple(
        &creds,
        &to_addr,
        &subject,
        text_body.as_deref(),
        html_body.as_deref(),
        cc_addr.as_deref(),
        bcc_addr.as_deref(),
        reply_to.as_deref(),
    )
    .await
    .context("failed to send draft")?;

    // ── Delete from IMAP Drafts folder + Copy to Sent ──
    if let Some(uid) = imap_uid {
        if !acct.imap_host.is_empty() {
            match imap::connect(&creds).await {
                Ok(mut client) => {
                    let drafts_folder = detect_drafts_folder(&mut client, &db, &acct.id)
                        .await
                        .map_err(|e| anyhow::anyhow!("drafts folder detection failed: {e}"))?
                        .unwrap_or_else(|| "Drafts".to_string());

                    if let Err(e) = imap::delete_message(&mut client, &drafts_folder, uid).await {
                        warn!(
                            "failed to delete draft from IMAP {} (UID {uid}): {e}",
                            drafts_folder
                        );
                    }

                    // Copy to Sent folder
                    let from = if let Some(ref display) = creds.account.display_name {
                        format!("{display} <{}>", creds.account.username)
                    } else {
                        creds.account.username.clone()
                    };
                    if let Ok((rfc822_bytes, _)) = build_rfc822_draft(
                        &from,
                        &to_addr,
                        Some(&subject),
                        text_body.as_deref(),
                        cc_addr.as_deref(),
                        None,
                    ) {
                        let sent_result = detect_sent_folder(&mut client, &db, &acct.id).await;
                        if let Ok(Some(sent_folder)) = sent_result {
                            if let Err(e) = imap::append_message(
                                &mut client,
                                &sent_folder,
                                "(\\Seen)",
                                &rfc822_bytes,
                            )
                            .await
                            {
                                warn!("failed to copy sent message to {sent_folder}: {e}");
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("failed to connect to IMAP to clean up sent draft: {e}");
                }
            }
        }
    }

    // ── Update local SQLite record ──
    if local_draft.is_some() {
        db.mark_draft_sent(id, Some(&message_id))
            .context("failed to mark draft as sent")?;
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "sent",
                "draft_id": id,
                "to": to_addr,
                "subject": subject,
                "message_id": message_id,
                "imap_draft_deleted": imap_uid.is_some(),
            })
        );
    } else {
        println!("Draft {id} sent to {to_addr}");
        println!("Subject: {subject}");
        println!("Message-ID: {message_id}");
    }

    Ok(())
}

// ─── draft discard ───────────────────────────────────────────────────────

#[tokio::main]
pub async fn run_discard(
    id: &str,
    json: bool,
    account: Option<&str>,
    backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    let is_imap_uid = id.parse::<u32>().is_ok();
    let local_draft = db.get_draft(id).context("failed to get draft")?;

    let imap_uid: Option<u32> = if let Some(ref d) = local_draft {
        d.imap_uid
    } else if is_imap_uid {
        Some(id.parse::<u32>().unwrap())
    } else {
        None
    };

    // ── Delete from IMAP Drafts folder (primary) ──
    if let Some(uid) = imap_uid {
        let passphrase = credential_store::get_or_create_passphrase(backend)
            .context("credential store error")?;

        let acct = match account {
            Some(a) => resolve_account(&db, Some(a))?,
            None => {
                if let Some(ref d) = local_draft {
                    db.get_account(&d.account_id)
                        .context("database error")?
                        .ok_or_else(|| {
                            anyhow::anyhow!("account not found for draft: {}", d.account_id)
                        })?
                } else {
                    let acct = db
                        .default_account()
                        .context("failed to query default account")?;
                    acct.ok_or_else(|| {
                        anyhow::anyhow!("no --account specified and no default account")
                    })?
                }
            }
        };

        if !acct.imap_host.is_empty() {
            let creds = db
                .get_account_with_credentials(&acct.id, &passphrase)
                .context("failed to decrypt credentials")?;

            match imap::connect(&creds).await {
                Ok(mut client) => {
                    let drafts_folder = detect_drafts_folder(&mut client, &db, &acct.id)
                        .await
                        .map_err(|e| anyhow::anyhow!("drafts folder detection failed: {e}"))?
                        .unwrap_or_else(|| "Drafts".to_string());

                    if let Err(e) = imap::delete_message(&mut client, &drafts_folder, uid).await {
                        warn!(
                            "failed to delete draft from IMAP {} (UID {uid}): {e}",
                            drafts_folder
                        );
                    }
                }
                Err(e) => {
                    warn!("failed to connect to IMAP to discard draft: {e}");
                }
            }
        }
    }

    // ── Delete local SQLite record (secondary) ──
    if local_draft.is_some() {
        let discarded = db.discard_draft(id).context("failed to discard draft")?;
        if !discarded {
            warn!("local draft {id} was not discardable (status may have changed)");
        }
    } else if !is_imap_uid {
        bail!("draft not found: {id}");
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "discard",
                "draft_id": id,
                "imap_deleted": imap_uid.is_some(),
                "local_deleted": local_draft.is_some(),
            })
        );
    } else {
        println!("Draft {id} discarded");
        if imap_uid.is_some() {
            println!("  IMAP: deleted from Drafts folder");
        }
    }

    Ok(())
}
