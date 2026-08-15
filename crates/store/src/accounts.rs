// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::crypto;
use crate::db::Database;
use crate::errors::{Result, StoreError};
use crate::models::{Account, AccountWithCredentials};
use rusqlite::params;
use uuid::Uuid;

impl Database {
    /// Create a new account with encrypted credentials.
    pub fn create_account(
        &self,
        name: &str,
        username: &str,
        password: &str,
        smtp_host: &str,
        smtp_port: u16,
        imap_host: &str,
        imap_port: u16,
        passphrase: &str,
    ) -> Result<Account> {
        let id = Uuid::new_v4().to_string();
        let domain = username.split('@').nth(1).unwrap_or("unknown").to_string();
        let encrypted_password = crypto::encrypt(password, passphrase)?;

        self.conn().execute(
            "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
             imap_host, imap_port, encrypted_password)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                name,
                username,
                domain,
                smtp_host,
                smtp_port,
                imap_host,
                imap_port,
                encrypted_password
            ],
        )?;

        self.get_account(&id)?
            .ok_or_else(|| StoreError::AccountNotFound(id))
    }

    /// Create or update an account with encrypted credentials.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_account_credentials(
        &self,
        name: &str,
        username: &str,
        password: &str,
        smtp_password: Option<&str>,
        smtp_host: &str,
        smtp_port: u16,
        imap_host: &str,
        imap_port: u16,
        passphrase: &str,
    ) -> Result<Account> {
        let domain = username.split('@').nth(1).unwrap_or("unknown").to_string();
        let encrypted_password = crypto::encrypt(password, passphrase)?;
        let encrypted_smtp_password = smtp_password
            .filter(|pw| *pw != password)
            .map(|pw| crypto::encrypt(pw, passphrase))
            .transpose()?;

        if let Some(existing) = self.find_account_by_email(username)? {
            self.conn().execute(
                "UPDATE accounts SET name = ?1, domain = ?2, smtp_host = ?3, smtp_port = ?4,
                 imap_host = ?5, imap_port = ?6, encrypted_password = ?7,
                 encrypted_smtp_password = ?8, encrypted_imap_password = NULL
                 WHERE id = ?9",
                params![
                    name,
                    domain,
                    smtp_host,
                    smtp_port,
                    imap_host,
                    imap_port,
                    encrypted_password,
                    encrypted_smtp_password,
                    existing.id
                ],
            )?;
            return self
                .get_account(&existing.id)?
                .ok_or_else(|| StoreError::AccountNotFound(existing.id));
        }

        let id = Uuid::new_v4().to_string();
        self.conn().execute(
            "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
             imap_host, imap_port, encrypted_password, encrypted_smtp_password)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                name,
                username,
                domain,
                smtp_host,
                smtp_port,
                imap_host,
                imap_port,
                encrypted_password,
                encrypted_smtp_password
            ],
        )?;

        self.get_account(&id)?
            .ok_or_else(|| StoreError::AccountNotFound(id))
    }

    /// List all accounts (without credentials).
    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port,
                    smtp_username, imap_username, display_name, signature_text, signature_html,
                    created_at
             FROM accounts ORDER BY created_at",
        )?;

        let accounts = stmt
            .query_map([], |row| {
                Ok(Account {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    username: row.get(2)?,
                    domain: row.get(3)?,
                    smtp_host: row.get(4)?,
                    smtp_port: row.get(5)?,
                    imap_host: row.get(6)?,
                    imap_port: row.get(7)?,
                    smtp_username: row.get(8)?,
                    imap_username: row.get(9)?,
                    display_name: row.get(10)?,
                    signature_text: row.get(11)?,
                    signature_html: row.get(12)?,
                    created_at: row.get(13)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(accounts)
    }

    /// Get a single account by ID (without credentials).
    pub fn get_account(&self, id: &str) -> Result<Option<Account>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port,
                    smtp_username, imap_username, display_name, signature_text, signature_html,
                    created_at
             FROM accounts WHERE id = ?1",
        )?;

        let account = stmt
            .query_row(params![id], |row| {
                Ok(Account {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    username: row.get(2)?,
                    domain: row.get(3)?,
                    smtp_host: row.get(4)?,
                    smtp_port: row.get(5)?,
                    imap_host: row.get(6)?,
                    imap_port: row.get(7)?,
                    smtp_username: row.get(8)?,
                    imap_username: row.get(9)?,
                    display_name: row.get(10)?,
                    signature_text: row.get(11)?,
                    signature_html: row.get(12)?,
                    created_at: row.get(13)?,
                })
            })
            .optional()?;

        Ok(account)
    }

    /// Find an account by email username.
    pub fn find_account_by_email(&self, email: &str) -> Result<Option<Account>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port,
                    smtp_username, imap_username, display_name, signature_text, signature_html,
                    created_at
             FROM accounts WHERE username = ?1",
        )?;

        let account = stmt
            .query_row(params![email], |row| {
                Ok(Account {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    username: row.get(2)?,
                    domain: row.get(3)?,
                    smtp_host: row.get(4)?,
                    smtp_port: row.get(5)?,
                    imap_host: row.get(6)?,
                    imap_port: row.get(7)?,
                    smtp_username: row.get(8)?,
                    imap_username: row.get(9)?,
                    display_name: row.get(10)?,
                    signature_text: row.get(11)?,
                    signature_html: row.get(12)?,
                    created_at: row.get(13)?,
                })
            })
            .optional()?;

        Ok(account)
    }

    /// Get account with decrypted credentials for transport operations.
    pub fn get_account_with_credentials(
        &self,
        id: &str,
        passphrase: &str,
    ) -> Result<AccountWithCredentials> {
        let account = self
            .get_account(id)?
            .ok_or_else(|| StoreError::AccountNotFound(id.to_string()))?;

        let encrypted_password: String = self.conn().query_row(
            "SELECT encrypted_password FROM accounts WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        let password = crypto::decrypt(&encrypted_password, passphrase)?;

        let smtp_password: Option<String> = self.conn().query_row(
            "SELECT encrypted_smtp_password FROM accounts WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        let smtp_password = smtp_password
            .as_deref()
            .map(|enc| crypto::decrypt(enc, passphrase))
            .transpose()?;

        let imap_password: Option<String> = self.conn().query_row(
            "SELECT encrypted_imap_password FROM accounts WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        let imap_password = imap_password
            .as_deref()
            .map(|enc| crypto::decrypt(enc, passphrase))
            .transpose()?;

        Ok(AccountWithCredentials {
            account,
            password,
            smtp_password,
            imap_password,
        })
    }

    /// Delete an account by ID, together with the address book derived from
    /// its mail.
    ///
    /// `contacts` and `address_history_state` are deleted explicitly and in
    /// the same transaction as the account row. Nothing enables
    /// `PRAGMA foreign_keys` on this connection, so no declared cascade fires
    /// and a plain `DELETE FROM accounts` would leave every address this
    /// account had ever corresponded with sitting in the database — names and
    /// addresses, keyed to an account the user believes they removed. One
    /// transaction so a failure part-way cannot leave the account gone and its
    /// address book behind.
    ///
    /// Scoped to `account_id`: every other account keeps its own rows. Other
    /// account-scoped tables are deliberately untouched here — this closes the
    /// address book the autocomplete work opened, and widening it is a
    /// separate decision.
    pub fn delete_account(&self, id: &str) -> Result<bool> {
        let tx = self.conn().unchecked_transaction()?;
        tx.execute("DELETE FROM contacts WHERE account_id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM address_history_state WHERE account_id = ?1",
            params![id],
        )?;
        let rows = tx.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(rows > 0)
    }

    /// Set (or clear) an account's signature fields.
    ///
    /// Passing `None` for a field clears it. Returns the updated account so
    /// callers can echo the stored state without a second query.
    pub fn set_account_signature(
        &self,
        id: &str,
        signature_text: Option<&str>,
        signature_html: Option<&str>,
    ) -> Result<Account> {
        let rows = self.conn().execute(
            "UPDATE accounts SET signature_text = ?1, signature_html = ?2 WHERE id = ?3",
            params![signature_text, signature_html, id],
        )?;
        if rows == 0 {
            return Err(StoreError::AccountNotFound(id.to_string()));
        }
        self.get_account(id)?
            .ok_or_else(|| StoreError::AccountNotFound(id.to_string()))
    }

    /// Get the default (first) account, or None if no accounts exist.
    pub fn default_account(&self) -> Result<Option<Account>> {
        let mut stmt = self.conn().prepare(
            "SELECT id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port,
                    smtp_username, imap_username, display_name, signature_text, signature_html,
                    created_at
             FROM accounts ORDER BY created_at LIMIT 1",
        )?;

        let account = stmt
            .query_row([], |row| {
                Ok(Account {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    username: row.get(2)?,
                    domain: row.get(3)?,
                    smtp_host: row.get(4)?,
                    smtp_port: row.get(5)?,
                    imap_host: row.get(6)?,
                    imap_port: row.get(7)?,
                    smtp_username: row.get(8)?,
                    imap_username: row.get(9)?,
                    display_name: row.get(10)?,
                    signature_text: row.get(11)?,
                    signature_html: row.get(12)?,
                    created_at: row.get(13)?,
                })
            })
            .optional()?;

        Ok(account)
    }
}

/// Extension trait for optional rusqlite query results.
trait OptionalExt<T> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for std::result::Result<T, rusqlite::Error> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(val) => Ok(Some(val)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_clear_account_signature() {
        let db = Database::open_memory().unwrap();
        let account = db
            .create_account(
                "Test",
                "test@example.com",
                "pw",
                "smtp.example.com",
                587,
                "imap.example.com",
                993,
                "passphrase",
            )
            .unwrap();
        assert_eq!(account.signature_text, None);

        let updated = db
            .set_account_signature(&account.id, Some("Tyler\nEnvelope"), None)
            .unwrap();
        assert_eq!(updated.signature_text.as_deref(), Some("Tyler\nEnvelope"));
        assert_eq!(updated.signature_html, None);

        let cleared = db.set_account_signature(&account.id, None, None).unwrap();
        assert_eq!(cleared.signature_text, None);
    }

    #[test]
    fn set_signature_unknown_account_errors() {
        let db = Database::open_memory().unwrap();
        let err = db
            .set_account_signature("nope", Some("x"), None)
            .unwrap_err();
        assert!(matches!(err, StoreError::AccountNotFound(_)));
    }

    #[test]
    fn create_and_list_accounts() {
        let db = Database::open_memory().unwrap();
        let passphrase = "test-passphrase";

        let account = db
            .create_account(
                "Test Gmail",
                "test@gmail.com",
                "fixture-login-value",
                "smtp.gmail.com",
                587,
                "imap.gmail.com",
                993,
                passphrase,
            )
            .unwrap();

        assert_eq!(account.username, "test@gmail.com");
        assert_eq!(account.domain, "gmail.com");

        let accounts = db.list_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, account.id);
    }

    #[test]
    fn upsert_account_credentials_updates_existing_without_plaintext_output() {
        let db = Database::open_memory().unwrap();
        let passphrase = "test-passphrase";

        let created = db
            .upsert_account_credentials(
                "Original",
                "user@example.com",
                "fixture-imap-value-one",
                Some("fixture-smtp-value-one"),
                "smtp.example.com",
                587,
                "imap.example.com",
                993,
                passphrase,
            )
            .unwrap();
        let updated = db
            .upsert_account_credentials(
                "Updated",
                "user@example.com",
                "fixture-imap-value-two",
                Some("fixture-smtp-value-two"),
                "smtp2.example.com",
                465,
                "imap2.example.com",
                993,
                passphrase,
            )
            .unwrap();

        assert_eq!(created.id, updated.id);
        assert_eq!(updated.name, "Updated");
        assert_eq!(updated.smtp_host, "smtp2.example.com");
        let creds = db
            .get_account_with_credentials(&updated.id, passphrase)
            .unwrap();
        assert_eq!(creds.effective_imap_password(), "fixture-imap-value-two");
        assert_eq!(creds.effective_smtp_password(), "fixture-smtp-value-two");
    }

    #[test]
    fn get_account_with_credentials() {
        let db = Database::open_memory().unwrap();
        let passphrase = "test-passphrase";

        let account = db
            .create_account(
                "Test",
                "user@example.com",
                "fixture-login-value",
                "smtp.example.com",
                587,
                "imap.example.com",
                993,
                passphrase,
            )
            .unwrap();

        let creds = db
            .get_account_with_credentials(&account.id, passphrase)
            .unwrap();
        assert_eq!(creds.password, "fixture-login-value");
        assert_eq!(creds.effective_smtp_username(), "user@example.com");
    }

    #[test]
    fn delete_account() {
        let db = Database::open_memory().unwrap();
        let account = db
            .create_account(
                "Test", "a@b.com", "pw", "s.b.com", 587, "i.b.com", 993, "pp",
            )
            .unwrap();

        assert!(db.delete_account(&account.id).unwrap());
        assert!(db.get_account(&account.id).unwrap().is_none());
    }

    /// Removing an account removes the address book derived from its mail.
    /// Nothing turns on `PRAGMA foreign_keys`, so no declared cascade runs and
    /// this has to be explicit: otherwise every correspondent's name and
    /// address stays in the database after the user removed the account they
    /// came from.
    #[test]
    fn delete_account_takes_its_address_history_with_it() {
        let db = Database::open_memory().unwrap();
        let account = db
            .create_account(
                "Test", "a@b.com", "pw", "s.b.com", 587, "i.b.com", 993, "pp",
            )
            .unwrap();

        db.upsert_contact(&crate::models::Contact {
            id: "c-1".into(),
            account_id: account.id.clone(),
            email: "correspondent@example.test".into(),
            name: Some("Correspondent".into()),
            tags: "[]".into(),
            notes: None,
            message_count: 3,
            first_seen: None,
            last_seen: None,
            created_at: "2026-01-01T00:00:00".into(),
            updated_at: "2026-01-01T00:00:00".into(),
        })
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO address_history_state
                    (account_id, source_version, last_thread_message_id)
                 VALUES (?1, 2, 41)",
                params![account.id],
            )
            .unwrap();

        assert!(db.delete_account(&account.id).unwrap());

        assert!(db.list_contacts(&account.id, None).unwrap().is_empty());
        let boundaries: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM address_history_state WHERE account_id = ?1",
                params![account.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(boundaries, 0, "the reconciliation boundary goes too");
    }

    /// And it takes nobody else's.
    #[test]
    fn delete_account_leaves_other_accounts_address_history_alone() {
        let db = Database::open_memory().unwrap();
        let doomed = db
            .create_account(
                "Doomed", "a@b.com", "pw", "s.b.com", 587, "i.b.com", 993, "pp",
            )
            .unwrap();
        let kept = db
            .create_account(
                "Kept", "c@d.com", "pw", "s.d.com", 587, "i.d.com", 993, "pp",
            )
            .unwrap();

        for (index, account_id) in [&doomed.id, &kept.id].into_iter().enumerate() {
            db.upsert_contact(&crate::models::Contact {
                id: format!("c-{index}"),
                account_id: account_id.clone(),
                email: "shared@example.test".into(),
                name: None,
                tags: "[]".into(),
                notes: None,
                message_count: 1,
                first_seen: None,
                last_seen: None,
                created_at: "2026-01-01T00:00:00".into(),
                updated_at: "2026-01-01T00:00:00".into(),
            })
            .unwrap();
            db.conn()
                .execute(
                    "INSERT INTO address_history_state
                        (account_id, source_version, last_thread_message_id)
                     VALUES (?1, 2, 7)",
                    params![account_id],
                )
                .unwrap();
        }

        assert!(db.delete_account(&doomed.id).unwrap());

        assert!(db.list_contacts(&doomed.id, None).unwrap().is_empty());
        assert_eq!(
            db.list_contacts(&kept.id, None).unwrap().len(),
            1,
            "the surviving account keeps its address book, shared address and all"
        );
        let boundaries: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM address_history_state WHERE account_id = ?1",
                params![kept.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(boundaries, 1);
        assert!(db.get_account(&kept.id).unwrap().is_some());
    }
}
