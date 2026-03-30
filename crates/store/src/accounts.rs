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

    /// Delete an account by ID.
    pub fn delete_account(&self, id: &str) -> Result<bool> {
        let rows = self
            .conn()
            .execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
        Ok(rows > 0)
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
    fn create_and_list_accounts() {
        let db = Database::open_memory().unwrap();
        let passphrase = "test-passphrase";

        let account = db
            .create_account(
                "Test Gmail",
                "test@gmail.com",
                "app-password-123",
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
    fn get_account_with_credentials() {
        let db = Database::open_memory().unwrap();
        let passphrase = "test-passphrase";

        let account = db
            .create_account(
                "Test",
                "user@example.com",
                "secret-password",
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
        assert_eq!(creds.password, "secret-password");
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
}
