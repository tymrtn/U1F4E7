// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! OAuth sign-ins, stored inline in the password column.
//!
//! This line cannot add tables, so an OAuth account's `encrypted_password`
//! holds its whole grant, encrypted like any password, behind
//! [`V1_GRANT_MARKER`]: `oauth2:v1:<json>`. A V2 build that shares the
//! database reads this form, and writes [`OAUTH_PASSWORD_SENTINEL`] with the
//! grant in its own `oauth_grants` table (schema 24) instead; this build reads
//! that table when it exists. Neither value is ever offered to a server as a
//! password.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::crypto;
use crate::db::Database;
use crate::errors::{Result, StoreError};
use crate::models::{Account, CachedToken, OAuthGrant};

/// The password of a V2 account whose grant is in V2's `oauth_grants`.
pub const OAUTH_PASSWORD_SENTINEL: &str = "oauth2:grant";
/// Prefix of this line's inline grant: `oauth2:v1:<json>`.
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

/// The inline JSON. V2 parses these field names, so they are a contract
/// between the two lines.
#[derive(Serialize, Deserialize)]
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

fn marker(g: &NewOAuthGrant<'_>) -> Result<String> {
    let json = serde_json::to_string(&V1Grant {
        provider: g.provider.into(),
        transport: g.transport.into(),
        client_id: g.client_id.into(),
        authority: g.authority.into(),
        scopes: g.scopes.into(),
        refresh_token: g.refresh_token.into(),
        access_token: g.access_token.map(Into::into),
        expires_at: g.access_expires_at,
    })?;
    Ok(format!("{V1_GRANT_MARKER}{json}"))
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
    /// Creates an OAuth account with its grant inline.
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
        self.create_account(
            name,
            username,
            &marker(new_grant)?,
            smtp_host,
            smtp_port,
            imap_host,
            imap_port,
            passphrase,
        )
    }

    /// Signs an existing account in with OAuth: converts a password account,
    /// or replaces the grant of an OAuth account (reauth).
    pub fn set_oauth_grant(
        &self,
        account_id: &str,
        new_grant: &NewOAuthGrant<'_>,
        passphrase: &str,
    ) -> Result<()> {
        let updated = self.conn().execute(
            "UPDATE accounts SET encrypted_password = ?1,
             encrypted_smtp_password = NULL, encrypted_imap_password = NULL
             WHERE id = ?2",
            params![
                crypto::encrypt(&marker(new_grant)?, passphrase)?,
                account_id
            ],
        )?;
        if updated == 0 {
            return Err(StoreError::AccountNotFound(account_id.to_string()));
        }
        Ok(())
    }

    /// Whether V2's `oauth_grants` table exists in this database.
    pub(crate) fn has_v2_grant_table(&self) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'oauth_grants'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// The grant behind a decrypted password, if that password is an OAuth
    /// placeholder. A V2 sentinel with no grant row fails loud: falling back
    /// to the sentinel as a password would send it to the server.
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
        if !self.has_v2_grant_table()? {
            return Err(StoreError::OAuthReauthRequired(username.to_string()));
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
    use rusqlite::Connection;

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

    fn stored_password(db: &Database, id: &str) -> String {
        let enc: String = db
            .conn()
            .query_row(
                "SELECT encrypted_password FROM accounts WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        crypto::decrypt(&enc, PASS).unwrap()
    }

    /// Adds V2's schema-24 `oauth_grants` table, as a shared database has it.
    fn add_v2_grant_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE oauth_grants (
                account_id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                transport TEXT NOT NULL,
                client_id TEXT NOT NULL,
                authority TEXT NOT NULL,
                scopes TEXT NOT NULL,
                encrypted_refresh_token TEXT NOT NULL,
                encrypted_access_token TEXT,
                access_expires_at TEXT,
                grant_version INTEGER NOT NULL DEFAULT 0,
                needs_reauth INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )
        .unwrap();
    }

    fn v2_sentinel_account(db: &Database) -> Account {
        db.create_account(
            "Gmail",
            "v2@gmail.com",
            OAUTH_PASSWORD_SENTINEL,
            "smtp.gmail.com",
            465,
            "imap.gmail.com",
            993,
            PASS,
        )
        .unwrap()
    }

    #[test]
    fn an_oauth_account_resolves_to_its_grant_and_no_password() {
        let db = Database::open_memory().unwrap();
        let account = oauth_account(&db);
        let creds = db.get_account_with_credentials(&account.id, PASS).unwrap();
        assert_eq!(
            creds.password, "",
            "the inline grant never leaves the store"
        );
        assert_eq!(creds.effective_imap_password(), "");
        assert_eq!(creds.effective_smtp_password(), "");
        let grant = creds.oauth.expect("grant resolved");
        assert_eq!(grant.refresh_token, "rt-1");
        assert_eq!(grant.transport, TRANSPORT_IMAP_XOAUTH2);
        assert_eq!(
            grant.cache.lock().unwrap().access_token.as_deref(),
            Some("at-1")
        );
    }

    /// V2 parses these exact field names; renaming one locks V2 out of
    /// accounts this line signs in.
    #[test]
    fn the_inline_grant_keeps_the_field_names_v2_reads() {
        let db = Database::open_memory().unwrap();
        let account = oauth_account(&db);
        let stored = stored_password(&db, &account.id);
        let json = stored
            .strip_prefix(V1_GRANT_MARKER)
            .expect("stored behind the marker");
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "access_token",
                "authority",
                "client_id",
                "expires_at",
                "provider",
                "refresh_token",
                "scopes",
                "transport"
            ]
        );
    }

    /// The minimal form V2's own test writes still parses here.
    #[test]
    fn a_minimal_inline_grant_is_read_and_never_returned_as_a_password() {
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
    fn a_v2_sentinel_without_its_table_asks_for_reauth() {
        let db = Database::open_memory().unwrap();
        let account = v2_sentinel_account(&db);
        let err = db
            .get_account_with_credentials(&account.id, PASS)
            .err()
            .expect("must not fall back to the sentinel as a password");
        assert!(
            matches!(err, StoreError::OAuthReauthRequired(ref u) if u == "v2@gmail.com"),
            "{err}"
        );
    }

    #[test]
    fn a_v2_sentinel_without_its_row_asks_for_reauth() {
        let db = Database::open_memory().unwrap();
        add_v2_grant_table(db.conn());
        let account = v2_sentinel_account(&db);
        let err = db
            .get_account_with_credentials(&account.id, PASS)
            .err()
            .expect("must not fall back to the sentinel as a password");
        assert!(matches!(err, StoreError::OAuthReauthRequired(_)), "{err}");
    }

    #[test]
    fn a_v2_grant_row_is_read() {
        let db = Database::open_memory().unwrap();
        add_v2_grant_table(db.conn());
        let account = v2_sentinel_account(&db);
        db.conn()
            .execute(
                "INSERT INTO oauth_grants (account_id, provider, transport, client_id,
                     authority, scopes, encrypted_refresh_token, encrypted_access_token,
                     access_expires_at)
                 VALUES (?1, 'google', 'imap_xoauth2', 'cid', 'https://accounts.google.com',
                     'https://mail.google.com/', ?2, ?3, '2026-10-07T10:00:00+00:00')",
                params![
                    account.id,
                    crypto::encrypt("rt-v2", PASS).unwrap(),
                    crypto::encrypt("at-v2", PASS).unwrap()
                ],
            )
            .unwrap();
        let creds = db.get_account_with_credentials(&account.id, PASS).unwrap();
        assert_eq!(creds.password, "");
        let grant = creds.oauth.unwrap();
        assert_eq!(grant.refresh_token, "rt-v2");
        let cache = grant.cache.lock().unwrap();
        assert_eq!(cache.access_token.as_deref(), Some("at-v2"));
        assert!(cache.expires_at.is_some());
    }

    #[test]
    fn a_password_account_converts_and_reauth_replaces_the_grant() {
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
        db.conn()
            .execute(
                "UPDATE accounts SET encrypted_smtp_password = ?1 WHERE id = ?2",
                params![crypto::encrypt("smtp-pw", PASS).unwrap(), account.id],
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
    }

    #[test]
    fn signing_in_a_missing_account_fails() {
        let db = Database::open_memory().unwrap();
        let err = db
            .set_oauth_grant("nope", &new_grant("rt"), PASS)
            .expect_err("no such account");
        assert!(matches!(err, StoreError::AccountNotFound(_)), "{err}");
    }

    #[test]
    fn deleting_the_account_deletes_a_v2_grant_row() {
        let db = Database::open_memory().unwrap();
        add_v2_grant_table(db.conn());
        let account = v2_sentinel_account(&db);
        db.conn()
            .execute(
                "INSERT INTO oauth_grants (account_id, provider, transport, client_id,
                     authority, scopes, encrypted_refresh_token)
                 VALUES (?1, 'google', 'imap_xoauth2', 'cid', 'a', 's', 'enc')",
                params![account.id],
            )
            .unwrap();
        assert!(db.delete_account(&account.id).unwrap());
        let left: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM oauth_grants", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn deleting_an_oauth_account_works_without_the_v2_table() {
        let db = Database::open_memory().unwrap();
        let account = oauth_account(&db);
        assert!(db.delete_account(&account.id).unwrap());
        assert!(db.find_account_by_email("you@gmail.com").unwrap().is_none());
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
