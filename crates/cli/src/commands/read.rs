// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{bail, Context, Result};

use super::common::setup_credentials;

#[tokio::main]
pub async fn run(uid: u32, folder: &str, account: Option<&str>, json: bool) -> Result<()> {
    let (_db, creds) = setup_credentials(account)?;

    let mut client = envelope_email_transport::imap::connect(&creds)
        .await
        .context("IMAP connection failed")?;

    let message = envelope_email_transport::imap::fetch_message(&mut client, folder, uid).await?;

    match message {
        Some(msg) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&msg)?);
            } else {
                println!("UID:      {}", msg.uid);
                if let Some(ref mid) = msg.message_id {
                    println!("ID:       {mid}");
                }
                println!("From:     {}", msg.from_addr);
                println!("To:       {}", msg.to_addr);
                if let Some(ref cc) = msg.cc_addr {
                    println!("CC:       {cc}");
                }
                println!("Subject:  {}", msg.subject);
                if let Some(ref date) = msg.date {
                    println!("Date:     {date}");
                }
                println!("Flags:    {}", msg.flags.join(", "));

                if !msg.attachments.is_empty() {
                    println!("Attachments:");
                    for att in &msg.attachments {
                        println!("  - {} ({}, {} bytes)", att.filename, att.content_type, att.size);
                    }
                }

                println!();
                if let Some(ref text) = msg.text_body {
                    println!("{text}");
                } else if let Some(ref html) = msg.html_body {
                    println!("[HTML body — use --json for raw content]");
                    // Print a simple stripped version
                    let stripped: String = html
                        .chars()
                        .fold((String::new(), false), |(mut acc, in_tag), c| {
                            if c == '<' {
                                (acc, true)
                            } else if c == '>' {
                                (acc, false)
                            } else if !in_tag {
                                acc.push(c);
                                (acc, false)
                            } else {
                                (acc, true)
                            }
                        })
                        .0;
                    println!("{stripped}");
                } else {
                    println!("[no body]");
                }
            }
        }
        None => bail!("message UID {uid} not found in {folder}"),
    }

    Ok(())
}
