// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::AccountsCmd;
use anyhow::{Context, Result, bail};
use envelope_email_store::Database;
use envelope_email_store::credential_store::{self, CredentialBackend};
use std::io::{self, Write};

pub fn run(cmd: AccountsCmd, json: bool, backend: CredentialBackend) -> Result<()> {
    match cmd {
        AccountsCmd::Add {
            email,
            password,
            name,
            smtp_host,
            imap_host,
            smtp_port,
            imap_port,
        } => add(
            &email, password, name, smtp_host, smtp_port, imap_host, imap_port, json, backend,
        ),
        AccountsCmd::List => list(json),
        AccountsCmd::Remove { id } => remove(&id, json),
    }
}

#[tokio::main]
async fn add(
    email: &str,
    password: Option<String>,
    name: Option<String>,
    smtp_host: Option<String>,
    smtp_port: Option<u16>,
    imap_host: Option<String>,
    imap_port: Option<u16>,
    json: bool,
    backend: CredentialBackend,
) -> Result<()> {
    let password = match password {
        Some(pw) => pw,
        None => prompt_password("Password: ")?,
    };

    let display_name = name.unwrap_or_else(|| email.to_string());

    let (smtp_host, smtp_port, imap_host, imap_port) = match (smtp_host, imap_host) {
        (Some(sh), Some(ih)) => (sh, smtp_port.unwrap_or(587), ih, imap_port.unwrap_or(993)),
        _ => {
            let domain = email
                .split('@')
                .nth(1)
                .context("invalid email address — missing @")?;

            eprintln!("Discovering mail servers for {domain}...");
            match envelope_email_transport::discover(domain).await {
                Ok(result) => {
                    let sp = smtp_port.unwrap_or(result.smtp_port);
                    let ip = imap_port.unwrap_or(result.imap_port);
                    eprintln!(
                        "Discovered SMTP: {}:{} (via {}), IMAP: {}:{} (via {})",
                        result.smtp_host,
                        sp,
                        result.smtp_source,
                        result.imap_host,
                        ip,
                        result.imap_source,
                    );
                    (result.smtp_host, sp, result.imap_host, ip)
                }
                Err(e) => {
                    eprintln!(
                        "Auto-discovery failed ({e}), falling back to defaults for {domain}."
                    );
                    let sh = format!("smtp.{domain}");
                    let ih = format!("imap.{domain}");
                    let sp = smtp_port.unwrap_or(587);
                    let ip = imap_port.unwrap_or(993);
                    eprintln!("  SMTP: {sh}:{sp}");
                    eprintln!("  IMAP: {ih}:{ip}");
                    eprintln!("  Override with --smtp-host / --imap-host if incorrect.");
                    (sh, sp, ih, ip)
                }
            }
        }
    };

    let passphrase = credential_store::get_or_create_passphrase(backend)
        .context("failed to access credential store for encryption")?;

    let db = Database::open_default().context("failed to open database")?;

    let account = db
        .create_account(
            &display_name,
            email,
            &password,
            &smtp_host,
            smtp_port,
            &imap_host,
            imap_port,
            &passphrase,
        )
        .context("failed to create account")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&account)?);
    } else {
        println!("Account added: {} ({})", account.username, account.id);
    }

    Ok(())
}

fn list(json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let accounts = db.list_accounts().context("failed to list accounts")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&accounts)?);
        return Ok(());
    }

    if accounts.is_empty() {
        println!(
            "No accounts configured. Add one with: envelope-email accounts add --email you@example.com"
        );
        return Ok(());
    }

    // Table output
    println!(
        "{:<36}  {:<30}  {:<20}  {}",
        "ID", "EMAIL", "DOMAIN", "CREATED"
    );
    println!("{}", "-".repeat(100));
    for acct in &accounts {
        println!(
            "{:<36}  {:<30}  {:<20}  {}",
            acct.id, acct.username, acct.domain, acct.created_at,
        );
    }
    println!("\n{} account(s)", accounts.len());

    Ok(())
}

fn remove(id_or_email: &str, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    // Try as UUID first, then as email
    let account = db
        .get_account(id_or_email)
        .context("database error")?
        .or_else(|| db.find_account_by_email(id_or_email).ok().flatten());

    let account = match account {
        Some(a) => a,
        None => bail!("account not found: {id_or_email}"),
    };

    let deleted = db
        .delete_account(&account.id)
        .context("failed to delete account")?;

    if !deleted {
        bail!("account not found: {}", account.id);
    }

    if json {
        println!(
            "{}",
            serde_json::json!({ "deleted": account.id, "email": account.username })
        );
    } else {
        println!("Removed account: {} ({})", account.username, account.id);
    }

    Ok(())
}

fn prompt_password(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    io::stderr().flush()?;

    let mut password = String::new();
    io::stdin()
        .read_line(&mut password)
        .context("failed to read password")?;

    let password = password.trim_end().to_string();
    if password.is_empty() {
        bail!("password cannot be empty");
    }

    Ok(password)
}
