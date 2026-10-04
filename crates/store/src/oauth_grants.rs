// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! OAuth sign-ins (migration 24's `oauth_grants`).
//!
//! An OAuth account's `encrypted_password` holds [`OAUTH_PASSWORD_SENTINEL`]
//! and its grant lives here. A public V1 build stores its grant inline in the
//! password column behind [`V1_GRANT_MARKER`] instead, because V1 cannot add
//! tables; this build reads both so a database shared between the two lines
//! never offers either value to a server as a password.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;

use crate::crypto;
use crate::db::Database;
use crate::errors::{Result, StoreError};
use crate::models::{Account, CachedToken, OAuthGrant};

/// Stored (encrypted) as the password of an account whose grant is in
/// `oauth_grants`. Never a real password, never sent anywhere.
pub const OAUTH_PASSWORD_SENTINEL: &str = "oauth2:grant";
/// Prefix of a public V1 build's inline grant: `oauth2:v1:<json>`.
pub const V1_GRANT_MARKER: &str = "oauth2:v1:";
/// IMAP and SMTP with SASL XOAUTH2 (Gmail).
pub const TRANSPORT_IMAP_XOAUTH2: &str = "imap_xoauth2";

/// True for a decrypted password that is really an OAuth placeholder.
pub fn is_oauth_password(password: &str) -> bool {
    password == OAUTH_PASSWORD_SENTINEL || password.starts_with(V1_GRANT_MARKER)
}

pub struct NewOAuthGrant<'a> {
    pub provider: &'a str,
    pub transport: &'a str,
    pub client_id: &'a str,
    pub authority: &'a str,
    pub scopes: &'a str,
    pub refresh_token: &'a str,
    pub access_token: Option<&'a str>,
    pub access_expires_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct V1Grant {
    provider: String,
    transport: String,
    client_id: String,
    #[serde(default)]
    authority: String,
    #[serde(default)]
    scopes: String,
    refresh_token: String,
    access_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
}

#[allow(clippy::too_many_arguments)]
fn grant(
    provider: String,
    transport: String,
    client_id: String,
    authority: String,
    scopes: String,
    refresh_token: String,
    access_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
) -> OAuthGrant {
    OAuthGrant {
        provider,
        transport,
        client_id,
        authority,
        scopes,
        refresh_token,
        cache: Arc::new(Mutex::new(CachedToken {
            access_token,
            expires_at,
        })),
    }
}

impl Database {
    /// Creates an OAuth account and its grant in one transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn create_oauth_account(
        &self,
        name: &str,
        username: &str,
        smtp_host: &str,
        smtp_port: u16,
        imap_host: &str,
        imap_port: u16,
        new_grant: &NewOAuthGrant<'_>,
        passphrase: &str,
    ) -> Result<Account> {
        let tx = self.conn().unchecked_transaction()?;
        let account = self.create_account(
            name,
            username,
            OAUTH_PASSWORD_SENTINEL,
            smtp_host,
            smtp_port,
            imap_host,
            imap_port,
            passphrase,
        )?;
        self.upsert_grant_row(&account.id, new_grant, passphrase)?;
        tx.commit()?;
        Ok(account)
    }

    /// Signs an existing account in with OAuth: converts a password account,
    /// or replaces the grant of an OAuth account (reauth). One transaction.
    pub fn set_oauth_grant(
        &self,
        account_id: &str,
        new_grant: &NewOAuthGrant<'_>,
        passphrase: &str,
    ) -> Result<()> {
        let tx = self.conn().unchecked_transaction()?;
        let updated = self.conn().execute(
            "UPDATE accounts SET encrypted_password = ?1,
             encrypted_smtp_password = NULL, encrypted_imap_password = NULL
             WHERE id = ?2",
            params![
                crypto::encrypt(OAUTH_PASSWORD_SENTINEL, passphrase)?,
                account_id
            ],
        )?;
        if updated == 0 {
            return Err(StoreError::AccountNotFound(account_id.to_string()));
        }
        self.upsert_grant_row(account_id, new_grant, passphrase)?;
        tx.commit()?;
        Ok(())
    }

    fn upsert_grant_row(
        &self,
        account_id: &str,
        g: &NewOAuthGrant<'_>,
        passphrase: &str,
    ) -> Result<()> {
        let refresh = crypto::encrypt(g.refresh_token, passphrase)?;
        let access = g
            .access_token
            .map(|t| crypto::encrypt(t, passphrase))
            .transpose()?;
        self.conn().execute(
            "INSERT INTO oauth_grants (account_id, provider, transport, client_id, authority,
                 scopes, encrypted_refresh_token, encrypted_access_token, access_expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(account_id) DO UPDATE SET
                 provider = excluded.provider,
                 transport = excluded.transport,
                 client_id = excluded.client_id,
                 authority = excluded.authority,
                 scopes = excluded.scopes,
                 encrypted_refresh_token = excluded.encrypted_refresh_token,
                 encrypted_access_token = excluded.encrypted_access_token,
                 access_expires_at = excluded.access_expires_at,
                 grant_version = grant_version + 1,
                 needs_reauth = 0,
                 last_error = NULL,
                 updated_at = datetime('now')",
            params![
                account_id,
                g.provider,
                g.transport,
                g.client_id,
                g.authority,
                g.scopes,
                refresh,
                access,
                g.access_expires_at.map(|t| t.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    /// Whether the account has a grant row, without decrypting anything.
    pub fn has_oauth_grant(&self, account_id: &str) -> Result<bool> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM oauth_grants WHERE account_id = ?1",
            params![account_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// The grant behind a decrypted password, if that password is an OAuth
    /// placeholder. A sentinel with no grant row fails loud: falling back to
    /// the sentinel as a password would send it to the server.
    pub(crate) fn resolve_oauth(
        &self,
        account_id: &str,
        username: &str,
        password: &str,
        passphrase: &str,
    ) -> Result<Option<OAuthGrant>> {
        if let Some(json) = password.strip_prefix(V1_GRANT_MARKER) {
            let v1: V1Grant = serde_json::from_str(json)?;
            return Ok(Some(grant(
                v1.provider,
                v1.transport,
                v1.client_id,
                v1.authority,
                v1.scopes,
                v1.refresh_token,
                v1.access_token,
                v1.expires_at,
            )));
        }
        if password != OAUTH_PASSWORD_SENTINEL {
            return Ok(None);
        }
        let row = self
            .conn()
            .query_row(
                "SELECT provider, transport, client_id, authority, scopes,
                        encrypted_refresh_token, encrypted_access_token, access_expires_at
                 FROM oauth_grants WHERE account_id = ?1",
                params![account_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((provider, transport, client_id, authority, scopes, refresh, access, expires)) =
            row
        else {
            return Err(StoreError::OAuthReauthRequired(username.to_string()));
        };
        let access_token = access
            .as_deref()
            .map(|enc| crypto::decrypt(enc, passphrase))
            .transpose()?;
        let expires_at = expires
            .as_deref()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc));
        Ok(Some(grant(
            provider,
            transport,
            client_id,
            authority,
            scopes,
            crypto::decrypt(&refresh, passphrase)?,
            access_token,
            expires_at,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASS: &str = "test-passphrase";

    fn new_grant(refresh: &str) -> NewOAuthGrant<'_> {
        NewOAuthGrant {
            provider: "google",
            transport: TRANSPORT_IMAP_XOAUTH2,
            client_id: "cid.apps.googleusercontent.com",
            authority: "https://accounts.google.com",
            scopes: "https://mail.google.com/ openid email",
            refresh_token: refresh,
            access_token: Some("at-1"),
            access_expires_at: None,
        }
    }

    fn oauth_account(db: &Database) -> Account {
        db.create_oauth_account(
            "Gmail",
            "you@gmail.com",
            "smtp.gmail.com",
            465,
            "imap.gmail.com",
            993,
            &new_grant("rt-1"),
            PASS,
        )
        .unwrap()
    }

    #[test]
    fn an_oauth_account_resolves_to_its_grant_and_no_password() {
        let db = Database::open_memory().unwrap();
        let account = oauth_account(&db);
        let creds = db.get_account_with_credentials(&account.id, PASS).unwrap();
        assert_eq!(creds.password, "", "the sentinel never leaves the store");
        assert_eq!(creds.effective_imap_password(), "");
        let grant = creds.oauth.expect("grant resolved");
        assert_eq!(grant.refresh_token, "rt-1");
        assert_eq!(grant.transport, TRANSPORT_IMAP_XOAUTH2);
        assert_eq!(
            grant.cache.lock().unwrap().access_token.as_deref(),
            Some("at-1")
        );
    }

    #[test]
    fn a_v1_inline_grant_is_read_and_never_returned_as_a_password() {
        let db = Database::open_memory().unwrap();
        let marker = format!(
            r#"{V1_GRANT_MARKER}{{"provider":"google","transport":"imap_xoauth2","client_id":"cid","refresh_token":"rt-v1"}}"#
        );
        let account = db
            .create_account(
                "Gmail",
                "v1@gmail.com",
                &marker,
                "smtp.gmail.com",
                465,
                "imap.gmail.com",
                993,
                PASS,
            )
            .unwrap();
        let creds = db.get_account_with_credentials(&account.id, PASS).unwrap();
        assert_eq!(creds.password, "");
        assert_eq!(creds.oauth.unwrap().refresh_token, "rt-v1");
    }

    #[test]
    fn a_sentinel_without_a_grant_asks_for_reauth() {
        let db = Database::open_memory().unwrap();
        let account = oauth_account(&db);
        db.conn().execute("DELETE FROM oauth_grants", []).unwrap();
        let err = db
            .get_account_with_credentials(&account.id, PASS)
            .err()
            .expect("must not fall back to the sentinel as a password");
        assert!(
            matches!(err, StoreError::OAuthReauthRequired(ref u) if u == "you@gmail.com"),
            "{err}"
        );
    }

    #[test]
    fn a_password_account_converts_and_reauth_bumps_the_version() {
        let db = Database::open_memory().unwrap();
        let account = db
            .create_account(
                "Gmail",
                "app@gmail.com",
                "app-password",
                "smtp.gmail.com",
                465,
                "imap.gmail.com",
                993,
                PASS,
            )
            .unwrap();
        db.set_oauth_grant(&account.id, &new_grant("rt-a"), PASS)
            .unwrap();
        db.set_oauth_grant(&account.id, &new_grant("rt-b"), PASS)
            .unwrap();
        let creds = db.get_account_with_credentials(&account.id, PASS).unwrap();
        assert_eq!(creds.password, "");
        assert!(creds.smtp_password.is_none() && creds.imap_password.is_none());
        assert_eq!(creds.oauth.unwrap().refresh_token, "rt-b");
        let version: i64 = db
            .conn()
            .query_row("SELECT grant_version FROM oauth_grants", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn deleting_the_account_deletes_its_grant() {
        let db = Database::open_memory().unwrap();
        let account = oauth_account(&db);
        assert!(db.delete_account(&account.id).unwrap());
        let left: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM oauth_grants", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn keychain_import_refuses_to_overwrite_an_oauth_account() {
        let db = Database::open_memory().unwrap();
        oauth_account(&db);
        let err = db
            .upsert_account_credentials(
                "Gmail",
                "you@gmail.com",
                "app-password",
                None,
                "smtp.gmail.com",
                465,
                "imap.gmail.com",
                993,
                PASS,
            )
            .expect_err("refused");
        assert!(matches!(err, StoreError::OAuthAccount(..)), "{err}");
    }

    #[test]
    fn grant_debug_output_never_contains_the_refresh_token() {
        let db = Database::open_memory().unwrap();
        let account = oauth_account(&db);
        let creds = db.get_account_with_credentials(&account.id, PASS).unwrap();
        let debug = format!("{:?}", creds.oauth.unwrap());
        assert!(!debug.contains("rt-1"), "{debug}");
    }
}
