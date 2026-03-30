// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Database-level encryption for account passwords.
//!
//! This module handles encrypting/decrypting individual values stored in SQLite.
//! For the master passphrase management (file vs keychain backends), see
//! `credential_store`.

use crate::errors::{Result, StoreError};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, OsRng},
};
use argon2::Argon2;
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use rand::RngCore;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

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
pub fn encrypt(plaintext: &str, passphrase: &str) -> Result<String> {
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
pub fn decrypt(encoded: &str, passphrase: &str) -> Result<String> {
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

/// Get the encryption passphrase from the OS keychain, or generate and store one.
///
/// **Deprecated**: Use `credential_store::get_or_create_passphrase(backend)` instead.
/// This function is kept for backward compatibility and delegates to the keychain
/// backend if the `keychain` feature is enabled, or the file backend otherwise.
pub fn get_or_create_passphrase() -> Result<String> {
    // Default: try file backend (works everywhere)
    crate::credential_store::get_or_create_passphrase(
        crate::credential_store::CredentialBackend::File,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let passphrase = "test-passphrase-123";
        let plaintext = "my-secret-password";
        let encrypted = encrypt(plaintext, passphrase).unwrap();
        let decrypted = decrypt(&encrypted, passphrase).unwrap();
        assert_eq!(plaintext, decrypted);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let encrypted = encrypt("secret", "correct-pass").unwrap();
        let result = decrypt(&encrypted, "wrong-pass");
        assert!(result.is_err());
    }
}
