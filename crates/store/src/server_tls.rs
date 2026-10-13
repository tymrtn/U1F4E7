// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! How an account secures its server connections, beyond host and port:
//! STARTTLS instead of implicit TLS for IMAP, and one pinned certificate the
//! account's servers must present (a local bridge such as Proton Mail Bridge
//! serves its own self-signed certificate).
//!
//! The settings live in a small JSON file per account in a `server-tls`
//! directory next to the database (`<data dir>/server-tls/<account id>.json`),
//! so the database schema is unchanged. An account without a file uses the
//! defaults: implicit TLS for IMAP (STARTTLS on port 143) and the public
//! certificate authorities.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::Database;
use crate::errors::{Result, StoreError};

pub(crate) const DIR_NAME: &str = "server-tls";

/// How the IMAP connection starts TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerTls {
    /// Set with `--imap-security`. Unset means STARTTLS on port 143 and
    /// implicit TLS on every other port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap_security: Option<ImapSecurity>,
    /// Set with `--trust-cert`: the DER bytes of the one certificate the
    /// account's IMAP, SMTP and ManageSieve servers must present. When set,
    /// it replaces the public certificate authorities for this account.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "pinned_cert_der_base64",
        with = "der_base64"
    )]
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

mod der_base64 {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(der: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match der {
            Some(bytes) => {
                s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
            }
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|text| {
                base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}

/// The settings file for `account_id` inside `dir`. Account ids are UUIDs;
/// anything else could escape the directory, so it is refused.
fn file_path(dir: &Path, account_id: &str) -> Result<PathBuf> {
    if account_id.is_empty()
        || !account_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(StoreError::Config(format!(
            "invalid account id for TLS settings: {account_id:?}"
        )));
    }
    Ok(dir.join(format!("{account_id}.json")))
}

fn load_from(dir: &Path, account_id: &str) -> Result<ServerTls> {
    let path = file_path(dir, account_id)?;
    let data = match std::fs::read_to_string(&path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ServerTls::default()),
        Err(e) => {
            return Err(StoreError::Config(format!(
                "cannot read {}: {e}",
                path.display()
            )));
        }
    };
    serde_json::from_str(&data).map_err(|e| {
        StoreError::Config(format!("corrupt TLS settings file {}: {e}", path.display()))
    })
}

fn save_to(dir: &Path, account_id: &str, settings: &ServerTls) -> Result<()> {
    let path = file_path(dir, account_id)?;
    if *settings == ServerTls::default() {
        return remove_from(dir, account_id);
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| StoreError::Config(format!("cannot create {}: {e}", dir.display())))?;
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(settings)?;
    std::fs::write(&tmp, data.as_bytes())
        .map_err(|e| StoreError::Config(format!("write {}: {e}", tmp.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            StoreError::Config(format!("set permissions on {}: {e}", tmp.display()))
        })?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        StoreError::Config(format!("replace {}: {e}", path.display()))
    })
}

fn remove_from(dir: &Path, account_id: &str) -> Result<()> {
    let path = file_path(dir, account_id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StoreError::Config(format!(
            "cannot remove {}: {e}",
            path.display()
        ))),
    }
}

impl Database {
    /// An account's TLS settings. An in-memory database has no settings
    /// directory, so every account there uses the defaults.
    pub fn server_tls(&self, account_id: &str) -> Result<ServerTls> {
        match self.server_tls_dir() {
            Some(dir) => load_from(dir, account_id),
            None => Ok(ServerTls::default()),
        }
    }

    /// Store an account's TLS settings. Saving the defaults deletes the file.
    pub fn set_server_tls(&self, account_id: &str, settings: &ServerTls) -> Result<()> {
        let dir = self.server_tls_dir().ok_or_else(|| {
            StoreError::Config("an in-memory database cannot keep TLS settings".to_string())
        })?;
        save_to(dir, account_id, settings)
    }

    pub(crate) fn remove_server_tls(&self, account_id: &str) -> Result<()> {
        match self.server_tls_dir() {
            Some(dir) => remove_from(dir, account_id),
            None => Ok(()),
        }
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
    fn missing_file_means_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(dir.path(), ID).unwrap(), ServerTls::default());
    }

    #[test]
    fn settings_survive_a_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        save_to(dir.path(), ID, &pinned()).unwrap();
        assert_eq!(load_from(dir.path(), ID).unwrap(), pinned());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        save_to(dir.path(), ID, &pinned()).unwrap();
        let mode = std::fs::metadata(dir.path().join(format!("{ID}.json")))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn saving_the_defaults_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        save_to(dir.path(), ID, &pinned()).unwrap();
        save_to(dir.path(), ID, &ServerTls::default()).unwrap();
        assert!(!dir.path().join(format!("{ID}.json")).exists());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(format!("{ID}.json")), "{not json").unwrap();
        let err = load_from(dir.path(), ID).unwrap_err().to_string();
        assert!(err.contains("corrupt TLS settings file"), "{err}");
    }

    #[test]
    fn an_account_id_that_could_leave_the_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["../x", "a/b", "", "a.json"] {
            assert!(load_from(dir.path(), id).is_err(), "{id:?}");
        }
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

    #[test]
    fn deleting_an_account_deletes_its_settings() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("envelope.db")).unwrap();
        let account = db
            .create_account(
                "Bridge",
                "you@proton.me",
                "bridge-password",
                "127.0.0.1",
                1025,
                "127.0.0.1",
                1143,
                "passphrase",
            )
            .unwrap();
        db.set_server_tls(&account.id, &pinned()).unwrap();
        let file = dir
            .path()
            .join(DIR_NAME)
            .join(format!("{}.json", account.id));
        assert!(file.exists());

        let creds = db
            .get_account_with_credentials(&account.id, "passphrase")
            .unwrap();
        assert_eq!(creds.server_tls, pinned());

        assert!(db.delete_account(&account.id).unwrap());
        assert!(!file.exists());
    }
}
