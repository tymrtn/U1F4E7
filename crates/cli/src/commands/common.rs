// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{bail, Context, Result};
use envelope_email_store::models::{Account, AccountWithCredentials};
use envelope_email_store::crypto::get_or_create_passphrase;
use envelope_email_store::Database;

/// Resolve an account from an optional --account flag.
/// If provided, looks up by ID or email. Otherwise returns the default account.
pub fn resolve_account(db: &Database, account_arg: Option<&str>) -> Result<Account> {
    match account_arg {
        Some(id_or_email) => {
            // Try ID first, then email
            let account = db
                .get_account(id_or_email)
                .context("database error")?
                .or_else(|| {
                    db.find_account_by_email(id_or_email)
                        .ok()
                        .flatten()
                });
            match account {
                Some(a) => Ok(a),
                None => bail!("account not found: {id_or_email}"),
            }
        }
        None => {
            let account = db
                .default_account()
                .context("failed to query default account")?;
            match account {
                Some(a) => Ok(a),
                None => bail!(
                    "no accounts configured. Add one with: envelope-email accounts add --email you@example.com"
                ),
            }
        }
    }
}

/// Open the database, get the passphrase, resolve account, and return credentials.
pub fn setup_credentials(account_arg: Option<&str>) -> Result<(Database, AccountWithCredentials)> {
    let db = Database::open_default().context("failed to open database")?;
    let passphrase = get_or_create_passphrase().context("keychain error")?;
    let acct = resolve_account(&db, account_arg)?;
    let creds = db
        .get_account_with_credentials(&acct.id, &passphrase)
        .context("failed to decrypt credentials")?;
    Ok((db, creds))
}
