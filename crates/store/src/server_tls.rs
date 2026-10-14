// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! How an account secures its server connections, beyond host and port:
//! STARTTLS instead of implicit TLS for IMAP, and one pinned certificate the
//! account's servers must present (a local bridge such as Proton Mail Bridge
//! serves its own self-signed certificate).
//!
//! The `account_server_tls` table is ensured additively outside
//! `PRAGMA user_version`, like `sieve_publications`: the V1 and V2 lines share
//! one migration sequence, so a numbered migration here would collide with
//! V2's. An account without a row uses the defaults: implicit TLS for IMAP
//! (STARTTLS on port 143) and the public certificate authorities. A row that
//! can't be read is an error, never the defaults, because the defaults would
//! trust every public authority where the account trusted one certificate.

use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::db::Database;
use crate::errors::{Result, StoreError};

pub(crate) fn ensure_schema(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS account_server_tls (
            account_id TEXT PRIMARY KEY,
            imap_security TEXT,
            pinned_cert_der BLOB
        );
        ",
    )?;
    Ok(())
}

/// How the IMAP connection starts TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImapSecurity {
    /// TLS from the first byte (port 993).
    Tls,
    /// Plain connection upgraded with STARTTLS before any credential is sent
    /// (port 143, and Proton Mail Bridge's default on 1143).
    Starttls,
}

impl ImapSecurity {
    /// The label mail clients use for this setting.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tls => "SSL/TLS",
            Self::Starttls => "STARTTLS",
        }
    }

    fn stored(self) -> &'static str {
        match self {
            Self::Tls => "tls",
            Self::Starttls => "starttls",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerTls {
    /// Set with `--imap-security`. Unset means STARTTLS on port 143 and
    /// implicit TLS on every other port.
    pub imap_security: Option<ImapSecurity>,
    /// Set with `--trust-cert`: the DER bytes of the one certificate the
    /// account's IMAP, SMTP and ManageSieve servers must present. When set,
    /// it replaces the public certificate authorities for this account.
    pub pinned_cert: Option<Vec<u8>>,
}

impl ServerTls {
    pub fn imap_security_for(&self, port: u16) -> ImapSecurity {
        self.imap_security.unwrap_or(if port == 143 {
            ImapSecurity::Starttls
        } else {
            ImapSecurity::Tls
        })
    }

    pub fn pinned_cert_sha256(&self) -> Option<String> {
        self.pinned_cert.as_deref().map(sha256_fingerprint)
    }
}

/// SHA-256 of a DER certificate as colon-separated uppercase hex, the form
/// `openssl x509 -noout -fingerprint -sha256` prints.
pub fn sha256_fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn corrupt(account_id: &str, why: impl std::fmt::Display) -> StoreError {
    StoreError::Config(format!(
        "corrupt TLS settings for account {account_id} (table account_server_tls in \
         envelope.db): {why}. Envelope will not connect this account until they are \
         fixed; removing the account and adding it again resets them"
    ))
}

impl Database {
    /// An account's TLS settings.
    pub fn server_tls(&self, account_id: &str) -> Result<ServerTls> {
        // A read-only open does not ensure the table. Without it no account
        // has stored settings.
        if !self.has_server_tls_table()? {
            return Ok(ServerTls::default());
        }
        let row = self
            .conn()
            .query_row(
                "SELECT imap_security, pinned_cert_der FROM account_server_tls
                 WHERE account_id = ?1",
                params![account_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| corrupt(account_id, e))?;
        let Some((imap_security, pinned_cert)) = row else {
            return Ok(ServerTls::default());
        };
        let imap_security = match imap_security.as_deref() {
            None => None,
            Some("tls") => Some(ImapSecurity::Tls),
            Some("starttls") => Some(ImapSecurity::Starttls),
            Some(other) => {
                return Err(corrupt(
                    account_id,
                    format!("unknown imap_security {other:?}"),
                ));
            }
        };
        if pinned_cert.as_ref().is_some_and(Vec::is_empty) {
            return Err(corrupt(account_id, "the pinned certificate is empty"));
        }
        Ok(ServerTls {
            imap_security,
            pinned_cert,
        })
    }

    /// Store an account's TLS settings. Saving the defaults deletes the row.
    pub fn set_server_tls(&self, account_id: &str, settings: &ServerTls) -> Result<()> {
        if *settings == ServerTls::default() {
            self.conn().execute(
                "DELETE FROM account_server_tls WHERE account_id = ?1",
                params![account_id],
            )?;
        } else {
            self.conn().execute(
                "INSERT INTO account_server_tls (account_id, imap_security, pinned_cert_der)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(account_id) DO UPDATE SET
                    imap_security = excluded.imap_security,
                    pinned_cert_der = excluded.pinned_cert_der",
                params![
                    account_id,
                    settings.imap_security.map(ImapSecurity::stored),
                    settings.pinned_cert,
                ],
            )?;
        }
        Ok(())
    }

    fn has_server_tls_table(&self) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'account_server_tls'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0b7e3c1e-7a51-4f0e-9d7a-8f3f2d1c0a99";

    fn pinned() -> ServerTls {
        ServerTls {
            imap_security: Some(ImapSecurity::Starttls),
            pinned_cert: Some(vec![0x30, 0x82, 0x01, 0x0a, 0xff]),
        }
    }

    #[test]
    fn an_account_without_a_row_uses_the_defaults() {
        let db = Database::open_memory().unwrap();
        assert_eq!(db.server_tls(ID).unwrap(), ServerTls::default());
    }

    #[test]
    fn settings_survive_a_round_trip_through_the_table() {
        let db = Database::open_memory().unwrap();
        db.set_server_tls(ID, &pinned()).unwrap();
        assert_eq!(db.server_tls(ID).unwrap(), pinned());

        let tls_only = ServerTls {
            imap_security: Some(ImapSecurity::Tls),
            pinned_cert: None,
        };
        db.set_server_tls(ID, &tls_only).unwrap();
        assert_eq!(db.server_tls(ID).unwrap(), tls_only);
    }

    #[test]
    fn saving_the_defaults_removes_the_row() {
        let db = Database::open_memory().unwrap();
        db.set_server_tls(ID, &pinned()).unwrap();
        db.set_server_tls(ID, &ServerTls::default()).unwrap();
        let rows: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM account_server_tls", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn without_the_table_every_account_uses_the_defaults() {
        // What a read-only open of a database this build never wrote sees.
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute_batch("DROP TABLE account_server_tls")
            .unwrap();
        assert_eq!(db.server_tls(ID).unwrap(), ServerTls::default());
    }

    #[test]
    fn imap_security_defaults_follow_the_port() {
        let defaults = ServerTls::default();
        assert_eq!(defaults.imap_security_for(993), ImapSecurity::Tls);
        assert_eq!(defaults.imap_security_for(143), ImapSecurity::Starttls);
        assert_eq!(defaults.imap_security_for(1143), ImapSecurity::Tls);
        assert_eq!(pinned().imap_security_for(1143), ImapSecurity::Starttls);
    }

    #[test]
    fn fingerprint_matches_the_openssl_format() {
        // `printf abc | openssl dgst -sha256`
        assert_eq!(
            sha256_fingerprint(b"abc"),
            "BA:78:16:BF:8F:01:CF:EA:41:41:40:DE:5D:AE:22:23:B0:03:61:A3:96:17:7A:9C:B4:10:FF:61:F2:00:15:AD"
        );
    }

    fn bridge_account(db: &Database) -> crate::models::Account {
        db.create_account(
            "Bridge",
            "you@proton.me",
            "bridge-password",
            "127.0.0.1",
            1025,
            "127.0.0.1",
            1143,
            "passphrase",
        )
        .unwrap()
    }

    #[test]
    fn the_pin_survives_a_backup_copy_of_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("envelope.db")).unwrap();
        let account = bridge_account(&db);
        db.set_server_tls(&account.id, &pinned()).unwrap();

        // Copied somewhere else, as a backup or a move to another machine.
        let elsewhere = tempfile::tempdir().unwrap();
        let copy = elsewhere.path().join("envelope.db");
        db.conn()
            .backup(rusqlite::DatabaseName::Main, &copy, None)
            .unwrap();
        let restored = Database::open(&copy).unwrap();

        assert_eq!(restored.server_tls(&account.id).unwrap(), pinned());
    }

    #[test]
    fn removing_and_adding_an_account_again_leaves_no_stale_pin() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("envelope.db")).unwrap();
        let first = bridge_account(&db);
        db.set_server_tls(&first.id, &pinned()).unwrap();

        assert!(db.delete_account(&first.id).unwrap());
        let rows: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM account_server_tls", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);

        let again = bridge_account(&db);
        assert_eq!(db.server_tls(&again.id).unwrap(), ServerTls::default());
    }

    #[test]
    fn a_corrupt_row_refuses_to_connect() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("envelope.db")).unwrap();
        let account = bridge_account(&db);
        db.set_server_tls(&account.id, &pinned()).unwrap();

        for corruption in [
            "UPDATE account_server_tls SET imap_security = 'ssl' WHERE account_id = ?1",
            "UPDATE account_server_tls SET pinned_cert_der = 'not a blob' WHERE account_id = ?1",
            "UPDATE account_server_tls SET pinned_cert_der = x'' WHERE account_id = ?1",
        ] {
            db.set_server_tls(&account.id, &pinned()).unwrap();
            db.conn()
                .execute(corruption, rusqlite::params![account.id])
                .unwrap();

            // Every connection starts from these credentials.
            let err = match db.get_account_with_credentials(&account.id, "passphrase") {
                Ok(_) => panic!("{corruption}: loaded an account with corrupt TLS settings"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains("corrupt TLS settings"), "{corruption}: {err}");
            assert!(err.contains(&account.id), "{corruption}: {err}");
        }
    }

    #[test]
    fn the_account_credentials_carry_its_settings() {
        let db = Database::open_memory().unwrap();
        let account = bridge_account(&db);
        db.set_server_tls(&account.id, &pinned()).unwrap();

        let creds = db
            .get_account_with_credentials(&account.id, "passphrase")
            .unwrap();

        assert_eq!(creds.server_tls, pinned());
    }
}
