// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result};
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_transport::imap;
use envelope_email_transport::smtp::SmtpSender;
use envelope_email_transport::unsubscribe;

use super::common::setup_credentials;

/// `envelope unsubscribe <uid>` — parse List-Unsubscribe and optionally execute.
///
/// Default is dry-run: shows what it would do. Pass `--confirm` to execute.
/// For mailto fallback, sends an empty unsubscribe email via SMTP.
#[tokio::main]
pub async fn run(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    confirm: bool,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let (_db, creds) = setup_credentials(account, backend)?;

    let mut client = imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    // Fetch message summary for display
    let msg = imap::fetch_message(&mut client, folder, uid)
        .await
        .context("failed to fetch message")?
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?;

    // Fetch List-Unsubscribe headers (separate fetch for raw headers)
    let (list_unsub, list_unsub_post) =
        imap::fetch_list_unsubscribe_headers(&mut client, folder, uid)
            .await
            .context("failed to fetch List-Unsubscribe headers")?;

    let list_unsub_str = match &list_unsub {
        Some(h) => h.as_str(),
        None => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "uid": uid,
                        "folder": folder,
                        "subject": msg.subject,
                        "from": msg.from_addr,
                        "status": "no_header",
                        "message": "No List-Unsubscribe header found",
                    })
                );
            } else {
                println!("UID {uid} ({folder})");
                println!("  From:    {}", msg.from_addr);
                println!("  Subject: {}", msg.subject);
                println!();
                println!("No List-Unsubscribe header found in this message.");
                println!("This sender does not support automated unsubscribe.");
            }
            return Ok(());
        }
    };

    let info = match unsubscribe::parse_list_unsubscribe(list_unsub_str, list_unsub_post.as_deref())
    {
        Some(info) => info,
        None => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "uid": uid,
                        "folder": folder,
                        "subject": msg.subject,
                        "from": msg.from_addr,
                        "raw_header": list_unsub_str,
                        "status": "parse_failed",
                        "message": "Could not parse List-Unsubscribe header",
                    })
                );
            } else {
                println!("UID {uid} ({folder})");
                println!("  From:    {}", msg.from_addr);
                println!("  Subject: {}", msg.subject);
                println!("  Header:  {list_unsub_str}");
                println!();
                println!("Could not parse List-Unsubscribe header.");
            }
            return Ok(());
        }
    };

    // For mailto: build an SMTP send closure
    // We capture the credentials to send via SMTP if needed
    let creds_for_smtp = &creds;
    let smtp_send: Box<
        dyn Fn(&str) -> std::result::Result<(), envelope_email_transport::SmtpError>,
    > = Box::new(|addr: &str| {
        // Use a blocking runtime to call the async SMTP sender
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async {
            SmtpSender::send_simple(
                creds_for_smtp,
                addr,
                "unsubscribe",
                Some("unsubscribe"),
                None,
                None,
                None,
                None,
            )
            .await
            .map(|_msg_id| ())
        })
    });

    let result = unsubscribe::execute_unsubscribe(&info, confirm, Some(smtp_send.as_ref())).await;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "uid": uid,
                "folder": folder,
                "subject": msg.subject,
                "from": msg.from_addr,
                "confirm": confirm,
                "info": {
                    "https_urls": info.https_urls,
                    "mailto_urls": info.mailto_urls,
                    "one_click_post": info.one_click_post,
                },
                "result": {
                    "method": result.method,
                    "url": result.url,
                    "status": result.status,
                    "message": result.message,
                },
            }))?
        );
    } else {
        println!("UID {uid} ({folder})");
        println!("  From:    {}", msg.from_addr);
        println!("  Subject: {}", msg.subject);
        println!();

        if !info.https_urls.is_empty() {
            println!("  HTTPS:   {}", info.https_urls.join(", "));
        }
        if !info.mailto_urls.is_empty() {
            println!("  Mailto:  {}", info.mailto_urls.join(", "));
        }
        if info.one_click_post {
            println!("  RFC 8058 one-click POST supported");
        }
        println!();

        match result.status.as_str() {
            "dry_run" => {
                println!("DRY RUN: {}", result.message);
                println!();
                println!("Pass --confirm to execute.");
            }
            "success" => {
                println!("SUCCESS: {}", result.message);
            }
            "failed" => {
                println!("FAILED: {}", result.message);
            }
            _ => {
                println!("{}: {}", result.status, result.message);
            }
        }
    }

    Ok(())
}
