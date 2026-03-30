// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Credential store abstraction.
//!
//! Two backends:
//! - **file** (default): AES-256-GCM encrypted credentials in
//!   `~/.config/envelope-email/credentials.json`, keyed by a master passphrase
//!   derived from `ENVELOPE_MASTER_KEY` env var or a machine-specific seed.
//! - **keychain**: OS keychain via the `keyring` crate (macOS Keychain,
//!   GNOME Keyring / KWallet on Linux). Requires the `keychain` cargo feature.

use crate::errors::{Result, StoreError};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, OsRng},
};
use argon2::Argon2;
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const SERVICE_NAME: &str = "envelope-email";
const MASTER_KEY_ENTRY: &str = "master-key";

/// Which credential backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialBackend {
    /// File-based encrypted store (default). Works everywhere.
    File,
    /// OS keychain (macOS Keychain, SecretService on Linux).
    /// Requires the `keychain` cargo feature.
    Keychain,
}

impl std::fmt::Display for CredentialBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File => write!(f, "file"),
            Self::Keychain => write!(f, "keychain"),
        }
    }
}

impl std::str::FromStr for CredentialBackend {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "file" => Ok(Self::File),
            "keychain" | "keyring" => Ok(Self::Keychain),
            other => Err(format!(
                "unknown credential store '{other}': expected 'file' or 'keychain'"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// File-based credential store
// ---------------------------------------------------------------------------

/// On-disk format for the credential file.
#[derive(Debug, Serialize, Deserialize, Default)]
struct CredentialFile {
    /// Master key verification blob (encrypted known plaintext).
    #[serde(default)]
    verify: Option<String>,
    /// Map of entry name -> encrypted value.
    #[serde(default)]
    entries: HashMap<String, String>,
}

fn credentials_path() -> PathBuf {
    let config_dir = dirs_next::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
    config_dir.join("envelope-email").join("credentials.json")
}

/// Derive the master passphrase for file-based storage.
///
/// Priority:
/// 1. `ENVELOPE_MASTER_KEY` environment variable (explicit)
/// 2. Machine-specific seed: SHA-256(hostname + username)
fn file_master_key() -> Result<String> {
    if let Ok(key) = std::env::var("ENVELOPE_MASTER_KEY") {
        if !key.is_empty() {
            return Ok(key);
        }
    }

    // Machine-specific seed
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown-host".to_string());
    let username = whoami::username();
    let seed = format!("envelope:{}:{}", hostname, username);

    // Use Argon2 with a fixed salt for deterministic derivation from the seed.
    let fixed_salt = b"envelope-email-machine-key-v1\0\0\0";
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(seed.as_bytes(), &fixed_salt[..16], &mut key)
        .map_err(|e| StoreError::Encryption(format!("machine key derivation failed: {e}")))?;

    Ok(B64.encode(key))
}

fn read_credential_file() -> Result<CredentialFile> {
    let path = credentials_path();
    if !path.exists() {
        return Ok(CredentialFile::default());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| StoreError::Config(format!("cannot read {}: {e}", path.display())))?;
    serde_json::from_str(&data).map_err(|e| {
        StoreError::Config(format!("corrupt credentials file {}: {e}", path.display()))
    })
}

fn write_credential_file(cf: &CredentialFile) -> Result<()> {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            StoreError::Config(format!(
                "cannot create config dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    let data = serde_json::to_string_pretty(cf)
        .map_err(|e| StoreError::Config(format!("serialize credentials: {e}")))?;
    std::fs::write(&path, data.as_bytes())
        .map_err(|e| StoreError::Config(format!("write {}: {e}", path.display())))?;

    // Restrict to owner-only on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms).map_err(|e| {
            StoreError::Config(format!("set permissions on {}: {e}", path.display()))
        })?;
    }

    Ok(())
}

/// Derive a 256-bit key from a passphrase using Argon2id.
fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| StoreError::Encryption(format!("key derivation failed: {e}")))?;
    Ok(key)
}

/// Encrypt plaintext using AES-256-GCM with Argon2id key derivation.
/// Returns base64-encoded: salt (16) || nonce (12) || ciphertext.
fn encrypt_value(plaintext: &str, passphrase: &str) -> Result<String> {
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let key = derive_key(passphrase, &salt)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| StoreError::Encryption(e.to_string()))?;

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| StoreError::Encryption(e.to_string()))?;

    let mut combined = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    combined.extend_from_slice(&salt);
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    Ok(B64.encode(&combined))
}

/// Decrypt a base64-encoded ciphertext (salt || nonce || ct) using AES-256-GCM.
fn decrypt_value(encoded: &str, passphrase: &str) -> Result<String> {
    let combined = B64
        .decode(encoded)
        .map_err(|e| StoreError::Decryption(format!("invalid base64: {e}")))?;

    if combined.len() < SALT_LEN + NONCE_LEN + 1 {
        return Err(StoreError::Decryption("ciphertext too short".into()));
    }

    let (salt, rest) = combined.split_at(SALT_LEN);
    let (nonce_bytes, ciphertext) = rest.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);

    let key = derive_key(passphrase, salt)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| StoreError::Decryption(e.to_string()))?;

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| StoreError::Decryption(e.to_string()))?;

    String::from_utf8(plaintext).map_err(|e| StoreError::Decryption(format!("invalid utf8: {e}")))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Get or create the master passphrase for encrypting account passwords in the DB.
///
/// This is the single entry point — the backend determines where the passphrase
/// is stored:
/// - **File**: derived from `ENVELOPE_MASTER_KEY` or machine seed (deterministic,
///   no external storage needed for the master key itself).
/// - **Keychain**: stored/retrieved via OS keychain.
pub fn get_or_create_passphrase(backend: CredentialBackend) -> Result<String> {
    match backend {
        CredentialBackend::File => file_get_or_create_passphrase(),
        CredentialBackend::Keychain => keychain_get_or_create_passphrase(),
    }
}

/// File backend: the passphrase is deterministic from the machine seed or env var,
/// but we also store a random passphrase in the credential file for DB encryption.
/// This way `get_or_create_passphrase` returns a stable passphrase across calls.
fn file_get_or_create_passphrase() -> Result<String> {
    let master = file_master_key()?;
    let mut cf = read_credential_file()?;

    // The master-key entry holds the actual random passphrase, encrypted with
    // the machine-derived master key.
    if let Some(encrypted) = cf.entries.get(MASTER_KEY_ENTRY) {
        return decrypt_value(encrypted, &master);
    }

    // First time: generate a random passphrase, encrypt, store.
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let passphrase = B64.encode(bytes);

    let encrypted = encrypt_value(&passphrase, &master)?;
    cf.entries.insert(MASTER_KEY_ENTRY.to_string(), encrypted);
    write_credential_file(&cf)?;

    Ok(passphrase)
}

/// Keychain backend: uses OS keyring.
fn keychain_get_or_create_passphrase() -> Result<String> {
    #[cfg(feature = "keychain")]
    {
        let entry = keyring::Entry::new(SERVICE_NAME, MASTER_KEY_ENTRY)
            .map_err(|e| StoreError::Keyring(e.to_string()))?;

        match entry.get_password() {
            Ok(pw) => Ok(pw),
            Err(keyring::Error::NoEntry) => {
                let mut bytes = [0u8; 32];
                OsRng.fill_bytes(&mut bytes);
                let passphrase = B64.encode(bytes);
                entry
                    .set_password(&passphrase)
                    .map_err(|e| StoreError::Keyring(e.to_string()))?;
                Ok(passphrase)
            }
            Err(e) => Err(StoreError::Keyring(e.to_string())),
        }
    }

    #[cfg(not(feature = "keychain"))]
    {
        Err(StoreError::Config(
            "keychain backend requires the 'keychain' cargo feature. \
             Rebuild with: cargo build --features keychain\n\
             Or use the default file backend: --credential-store file"
                .to_string(),
        ))
    }
}

/// Migrate an existing keychain passphrase to the file backend.
/// Returns Ok(true) if migration happened, Ok(false) if nothing to migrate.
#[allow(dead_code)]
pub fn migrate_keychain_to_file() -> Result<bool> {
    #[cfg(feature = "keychain")]
    {
        let entry = keyring::Entry::new(SERVICE_NAME, MASTER_KEY_ENTRY)
            .map_err(|e| StoreError::Keyring(e.to_string()))?;

        match entry.get_password() {
            Ok(keychain_passphrase) => {
                // Store it in the file backend
                let master = file_master_key()?;
                let mut cf = read_credential_file()?;
                let encrypted = encrypt_value(&keychain_passphrase, &master)?;
                cf.entries.insert(MASTER_KEY_ENTRY.to_string(), encrypted);
                write_credential_file(&cf)?;
                Ok(true)
            }
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(StoreError::Keyring(e.to_string())),
        }
    }

    #[cfg(not(feature = "keychain"))]
    {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let passphrase = "test-passphrase-123";
        let plaintext = "my-secret-password";
        let encrypted = encrypt_value(plaintext, passphrase).unwrap();
        let decrypted = decrypt_value(&encrypted, passphrase).unwrap();
        assert_eq!(plaintext, decrypted);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let encrypted = encrypt_value("secret", "correct-pass").unwrap();
        let result = decrypt_value(&encrypted, "wrong-pass");
        assert!(result.is_err());
    }

    #[test]
    fn file_master_key_is_deterministic() {
        // Set env var for deterministic testing
        // SAFETY: single-threaded test context
        unsafe {
            std::env::set_var("ENVELOPE_MASTER_KEY", "test-master-key-1234");
        }
        let k1 = file_master_key().unwrap();
        let k2 = file_master_key().unwrap();
        assert_eq!(k1, k2);
        unsafe {
            std::env::remove_var("ENVELOPE_MASTER_KEY");
        }
    }

    #[test]
    fn credential_backend_parse() {
        assert_eq!(
            "file".parse::<CredentialBackend>().unwrap(),
            CredentialBackend::File
        );
        assert_eq!(
            "keychain".parse::<CredentialBackend>().unwrap(),
            CredentialBackend::Keychain
        );
        assert_eq!(
            "keyring".parse::<CredentialBackend>().unwrap(),
            CredentialBackend::Keychain
        );
        assert!("invalid".parse::<CredentialBackend>().is_err());
    }
}
