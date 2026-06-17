// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::commands::ui;
use crate::{AccountsCmd, SignatureCmd};
use anyhow::{Context, Result, bail};
use envelope_email_store::credential_store::{self, CredentialBackend};
use envelope_email_store::{Account, Database};
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
        AccountsCmd::SetupInstructions { account, client } => {
            setup_instructions(&account, &client, json)
        }
        AccountsCmd::Remove { id } => remove(&id, json),
        AccountsCmd::ImportKeychain {
            email,
            name,
            imap_host,
            smtp_host,
            imap_port,
            smtp_port,
            confirm_read,
            import,
        } => crate::commands::keychain_import::run(
            &email,
            name,
            imap_host,
            smtp_host,
            imap_port,
            smtp_port,
            confirm_read,
            import,
            json,
            backend,
        ),
        AccountsCmd::Signature { subcommand } => signature(subcommand, json),
    }
}

/// Resolve an account by UUID or email, or error.
fn resolve_account(db: &Database, id_or_email: &str) -> Result<Account> {
    let account = db
        .get_account(id_or_email)
        .context("database error")?
        .or_else(|| db.find_account_by_email(id_or_email).ok().flatten());
    account.ok_or_else(|| anyhow::anyhow!("account not found: {id_or_email}"))
}

/// View or update an account's outbound signature(s).
///
/// Signatures are not secret material, so they may be printed. `set` merges
/// with the stored values so updating one field never clobbers the other.
fn signature(cmd: SignatureCmd, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    match cmd {
        SignatureCmd::Show { account } => {
            let acct = resolve_account(&db, &account)?;
            emit_signature(&acct, json, "shown")
        }
        SignatureCmd::Set {
            account,
            text,
            html,
            text_file,
            html_file,
        } => {
            let acct = resolve_account(&db, &account)?;

            let new_text = match (text, text_file) {
                (Some(t), _) => Some(t),
                (None, Some(f)) => Some(
                    std::fs::read_to_string(&f)
                        .with_context(|| format!("failed to read --text-file: {f}"))?,
                ),
                (None, None) => acct.signature_text.clone(),
            };
            let new_html = match (html, html_file) {
                (Some(h), _) => Some(h),
                (None, Some(f)) => Some(
                    std::fs::read_to_string(&f)
                        .with_context(|| format!("failed to read --html-file: {f}"))?,
                ),
                (None, None) => acct.signature_html.clone(),
            };

            if new_text.is_none() && new_html.is_none() {
                bail!(
                    "nothing to set: provide --text/--text-file and/or --html/--html-file (use `signature clear` to clear)"
                );
            }

            let updated = db
                .set_account_signature(&acct.id, new_text.as_deref(), new_html.as_deref())
                .context("failed to update signature")?;
            emit_signature(&updated, json, "updated")
        }
        SignatureCmd::Clear {
            account,
            text,
            html,
        } => {
            let acct = resolve_account(&db, &account)?;
            // No specific field flags means clear both.
            let clear_both = !text && !html;
            let new_text = if text || clear_both {
                None
            } else {
                acct.signature_text.clone()
            };
            let new_html = if html || clear_both {
                None
            } else {
                acct.signature_html.clone()
            };
            let updated = db
                .set_account_signature(&acct.id, new_text.as_deref(), new_html.as_deref())
                .context("failed to clear signature")?;
            emit_signature(&updated, json, "cleared")
        }
    }
}

fn emit_signature(account: &Account, json: bool, status: &str) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": status,
                "account_id": account.id,
                "email": account.username,
                "signature_text": account.signature_text,
                "signature_html": account.signature_html,
                "ui": ui::account_ui(&account.id),
            })
        );
    } else {
        println!(
            "Signature {status} for {} ({})",
            account.username, account.id
        );
        match account.signature_text.as_deref() {
            Some(t) => println!("signature_text:\n{t}"),
            None => println!("signature_text: (none)"),
        }
        match account.signature_html.as_deref() {
            Some(h) => println!("signature_html:\n{h}"),
            None => println!("signature_html: (none)"),
        }
    }
    Ok(())
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
        let value = ui::with_ui(&account, ui::account_ui(&account.id));
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("Account added: {} ({})", account.username, account.id);
    }

    Ok(())
}

fn list(json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let accounts = db.list_accounts().context("failed to list accounts")?;

    if json {
        let enriched: Vec<serde_json::Value> = accounts
            .iter()
            .map(|a| ui::with_ui(a, ui::account_ui(&a.id)))
            .collect();
        println!("{}", serde_json::to_string_pretty(&enriched)?);
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

/// Map a well-known mail port to its transport security. Returns a stable
/// string used in both JSON and human output. Defaults conservatively to TLS
/// for unknown ports so setup guidance never suggests an unencrypted client.
fn imap_security_for_port(port: u16) -> &'static str {
    match port {
        993 => "SSL/TLS",
        143 => "STARTTLS",
        _ => "SSL/TLS",
    }
}

fn smtp_security_for_port(port: u16) -> &'static str {
    match port {
        465 => "SSL/TLS",
        587 => "STARTTLS",
        25 => "STARTTLS",
        _ => "STARTTLS",
    }
}

fn setup_instructions(account_ref: &str, client: &str, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    let account = db
        .get_account(account_ref)
        .context("database error")?
        .or_else(|| db.find_account_by_email(account_ref).ok().flatten());

    let account = match account {
        Some(a) => a,
        None => bail!("account not found: {account_ref}"),
    };

    let imap_username = account
        .imap_username
        .as_deref()
        .unwrap_or(&account.username)
        .to_string();
    let smtp_username = account
        .smtp_username
        .as_deref()
        .unwrap_or(&account.username)
        .to_string();
    let imap_security = imap_security_for_port(account.imap_port);
    let smtp_security = smtp_security_for_port(account.smtp_port);

    if json {
        // Non-secret settings only. The account password lives in Envelope's
        // encrypted credential store and is intentionally never emitted here.
        let value = serde_json::json!({
            "account_id": account.id,
            "email": account.username,
            "display_name": account.display_name.as_deref().unwrap_or(&account.name),
            "client": client,
            "imap": {
                "host": account.imap_host,
                "port": account.imap_port,
                "username": imap_username,
                "security": imap_security,
            },
            "smtp": {
                "host": account.smtp_host,
                "port": account.smtp_port,
                "username": smtp_username,
                "security": smtp_security,
            },
            "password": "stored in Envelope credential store; not printed",
            "ui": ui::account_ui(&account.id),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!("Mail client setup for {} ({})", account.username, client);
    println!("Account type:        IMAP");
    println!("Email / username:    {}", account.username);
    println!();
    println!("Incoming mail (IMAP)");
    println!("  Host:      {}", account.imap_host);
    println!("  Port:      {}", account.imap_port);
    println!("  Username:  {imap_username}");
    println!("  Security:  {imap_security}");
    println!();
    println!("Outgoing mail (SMTP)");
    println!("  Host:      {}", account.smtp_host);
    println!("  Port:      {}", account.smtp_port);
    println!("  Username:  {smtp_username}");
    println!("  Security:  {smtp_security}");
    println!();
    println!(
        "Password:  stored in Envelope's encrypted credential store and not printed here.\n           Use your existing app/mailbox password when the client prompts."
    );

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
            serde_json::json!({
                "deleted": account.id,
                "email": account.username,
                "ui": ui::root_ui(),
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imap_security_mapping() {
        assert_eq!(imap_security_for_port(993), "SSL/TLS");
        assert_eq!(imap_security_for_port(143), "STARTTLS");
        // Unknown ports default to TLS, never plaintext.
        assert_eq!(imap_security_for_port(1234), "SSL/TLS");
    }

    #[test]
    fn smtp_security_mapping() {
        assert_eq!(smtp_security_for_port(465), "SSL/TLS");
        assert_eq!(smtp_security_for_port(587), "STARTTLS");
        assert_eq!(smtp_security_for_port(25), "STARTTLS");
        assert_eq!(smtp_security_for_port(9999), "STARTTLS");
    }
}
