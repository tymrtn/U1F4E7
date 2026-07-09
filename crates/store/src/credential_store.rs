// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Credential store abstraction.
//!
//! Two backends:
//! - **file** (default): AES-256-GCM encrypted credentials in
//!   `~/.config/envelope-email/credentials.json`, keyed by a master key derived
//!   from (in precedence order) the `ENVELOPE_MASTER_KEY` env var, the
//!   `ENVELOPE_MASTER_PASSPHRASE_FILE` env var (path to a `0600` passphrase
//!   file), an interactive passphrase verified against a stored Argon2
//!   verifier, or — only when explicitly opted in — a machine-derived key.
//! - **keychain**: OS keychain via the `keyring` crate (macOS Keychain,
//!   GNOME Keyring / KWallet on Linux). Requires the `keychain` cargo feature.
//!
//! The master key never encrypts account passwords directly. It encrypts a
//! single random *DB passphrase* stored under [`MASTER_KEY_ENTRY`]; that DB
//! passphrase is what the `crypto` module uses for the SQLite credential
//! columns. Rekeying therefore only re-wraps that one entry — account rows in
//! the database are never touched.

use crate::errors::{Result, StoreError};
use crate::paths;
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, OsRng},
};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const SERVICE_NAME: &str = "envelope-email";
const MASTER_KEY_ENTRY: &str = "master-key";

const ENV_MASTER_KEY: &str = "ENVELOPE_MASTER_KEY";
const ENV_PASSPHRASE_FILE: &str = "ENVELOPE_MASTER_PASSPHRASE_FILE";

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
// Interactive prompt injection
// ---------------------------------------------------------------------------

/// How the file backend obtains an interactive passphrase.
///
/// The store crate never touches the terminal directly. Callers (the CLI) wire
/// a real no-echo prompt; tests inject deterministic values. This is the single
/// injection seam that lets us exercise the interactive paths without faking a
/// TTY.
pub trait PassphrasePrompter {
    /// True when an interactive prompt is possible (a TTY is attached). When
    /// false the file backend must fail loud rather than prompt.
    fn is_interactive(&self) -> bool;

    /// Prompt once for an existing passphrase (unlock).
    fn prompt_unlock(&self) -> Result<String>;

    /// Prompt twice to establish a new passphrase; returns it only if the two
    /// entries match and are non-empty.
    fn prompt_new(&self) -> Result<String>;
}

/// A prompter that always refuses — the default for non-interactive callers and
/// for paths (like `quickstart`) that must never trigger first-time setup.
pub struct NonInteractive;

impl PassphrasePrompter for NonInteractive {
    fn is_interactive(&self) -> bool {
        false
    }
    fn prompt_unlock(&self) -> Result<String> {
        Err(StoreError::Config(non_interactive_remediation()))
    }
    fn prompt_new(&self) -> Result<String> {
        Err(StoreError::Config(non_interactive_remediation()))
    }
}

fn non_interactive_remediation() -> String {
    format!(
        "no passphrase available and no interactive terminal to prompt for one.\n\
         Set one of the following before running:\n\
         - export {ENV_MASTER_KEY}=<key>            (raw master key)\n\
         - export {ENV_PASSPHRASE_FILE}=<path>      (0600 file holding the passphrase)\n\
         Or run interactively once (`envelope accounts add ...`) to establish a passphrase,\n\
         or opt into the legacy machine key with `--insecure-machine-key`."
    )
}

// ---------------------------------------------------------------------------
// File-based credential store
// ---------------------------------------------------------------------------

/// On-disk format for the credential file.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct CredentialFile {
    /// Argon2 PHC verifier for the master passphrase. Present iff the file was
    /// created with a real (env-file or interactive) passphrase. Absent for
    /// legacy files created under the machine-derived key.
    #[serde(default)]
    verify: Option<String>,
    /// Map of entry name -> encrypted value.
    #[serde(default)]
    entries: HashMap<String, String>,
}

/// The resolved master key. (The source is used only for the inline legacy
/// warning at resolution time, so it is not carried on the struct.)
struct ResolvedMaster {
    key: String,
}

/// Read the passphrase from `ENVELOPE_MASTER_PASSPHRASE_FILE`, enforcing `0600`.
///
/// The trailing newline is stripped. Refuses (loudly) if the file is readable
/// by group or other — matching the systemd `LoadCredential` threat model.
fn read_passphrase_file(path: &str) -> Result<String> {
    let meta = std::fs::metadata(path).map_err(|e| {
        StoreError::Config(format!(
            "{ENV_PASSPHRASE_FILE} points at {path}, which cannot be read: {e}"
        ))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(StoreError::Config(format!(
                "{ENV_PASSPHRASE_FILE} at {path} is group/world-accessible (mode {mode:o}); \
                 refusing to read it.\nFix with: chmod 600 {path}"
            )));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = &meta;
    }

    let raw = std::fs::read_to_string(path).map_err(|e| {
        StoreError::Config(format!("cannot read {ENV_PASSPHRASE_FILE} at {path}: {e}"))
    })?;
    let pass = raw.strip_suffix('\n').unwrap_or(&raw);
    let pass = pass.strip_suffix('\r').unwrap_or(pass);
    if pass.is_empty() {
        return Err(StoreError::Config(format!(
            "{ENV_PASSPHRASE_FILE} at {path} is empty"
        )));
    }
    Ok(pass.to_string())
}

/// Derive the machine-specific master key (legacy behavior). Kept for
/// backwards-compatible reads and explicit `--insecure-machine-key` opt-in.
fn machine_master_key() -> Result<String> {
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown-host".to_string());
    let username = whoami::username();
    let seed = format!("envelope:{}:{}", hostname, username);

    let fixed_salt = b"envelope-email-machine-key-v1\0\0\0";
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(seed.as_bytes(), &fixed_salt[..16], &mut key)
        .map_err(|e| StoreError::Encryption(format!("machine key derivation failed: {e}")))?;

    Ok(B64.encode(key))
}

/// Compute an Argon2 PHC verifier string for a passphrase (random salt).
fn make_verifier(passphrase: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(passphrase.as_bytes(), &salt)
        .map_err(|e| StoreError::Encryption(format!("verifier hash failed: {e}")))?;
    Ok(hash.to_string())
}

/// Check a passphrase against a stored PHC verifier. Returns true on match,
/// false on mismatch, and an error only if the stored verifier is malformed.
fn verify_passphrase(passphrase: &str, verifier: &str) -> Result<bool> {
    let parsed = PasswordHash::new(verifier)
        .map_err(|e| StoreError::Config(format!("corrupt master-key verifier: {e}")))?;
    Ok(Argon2::default()
        .verify_password(passphrase.as_bytes(), &parsed)
        .is_ok())
}

fn read_credential_file() -> Result<CredentialFile> {
    let path = paths::credential_file_path();
    if !path.exists() {
        return Ok(CredentialFile::default());
    }
    let data = std::fs::read_to_string(&path)
        .map_err(|e| StoreError::Config(format!("cannot read {}: {e}", path.display())))?;
    serde_json::from_str(&data).map_err(|e| {
        StoreError::Config(format!("corrupt credentials file {}: {e}", path.display()))
    })
}

/// Serialize a credential file to bytes with pretty JSON.
fn serialize_credential_file(cf: &CredentialFile) -> Result<String> {
    serde_json::to_string_pretty(cf)
        .map_err(|e| StoreError::Config(format!("serialize credentials: {e}")))
}

#[cfg(unix)]
fn set_owner_only(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .map_err(|e| StoreError::Config(format!("set permissions on {}: {e}", path.display())))
}

fn write_credential_file(cf: &CredentialFile) -> Result<()> {
    let path = paths::credential_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            StoreError::Config(format!(
                "cannot create config dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    let data = serialize_credential_file(cf)?;
    std::fs::write(&path, data.as_bytes())
        .map_err(|e| StoreError::Config(format!("write {}: {e}", path.display())))?;

    #[cfg(unix)]
    set_owner_only(&path)?;

    Ok(())
}

/// Atomically replace the credential file: write a sibling temp file, fsync,
/// set `0600`, then rename over the target. On any failure before the rename,
/// the original file is untouched and the temp file is cleaned up.
fn write_credential_file_atomic(cf: &CredentialFile) -> Result<()> {
    let path = paths::credential_file_path();
    let parent = path
        .parent()
        .ok_or_else(|| StoreError::Config("credential file has no parent directory".into()))?;
    std::fs::create_dir_all(parent).map_err(|e| {
        StoreError::Config(format!(
            "cannot create config dir {}: {e}",
            parent.display()
        ))
    })?;

    let data = serialize_credential_file(cf)?;

    let mut tmp = path.clone();
    let pid = std::process::id();
    tmp.set_file_name(format!(".credentials.json.tmp.{pid}"));

    // Write + fsync the temp file, then set perms, then rename. Clean up on any
    // error so a failed rekey never leaves a partial temp behind.
    let write_result = (|| -> Result<()> {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)
            .map_err(|e| StoreError::Config(format!("create temp {}: {e}", tmp.display())))?;
        f.write_all(data.as_bytes())
            .map_err(|e| StoreError::Config(format!("write temp {}: {e}", tmp.display())))?;
        f.sync_all()
            .map_err(|e| StoreError::Config(format!("fsync temp {}: {e}", tmp.display())))?;
        drop(f);
        #[cfg(unix)]
        set_owner_only(&tmp)?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        StoreError::Config(format!("atomic rename to {}: {e}", path.display()))
    })?;

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
// Master key resolution
// ---------------------------------------------------------------------------

/// Resolve the master key for READ/unlock, given an already-loaded credential
/// file. Never prompts to *create* a passphrase and never mutates anything.
///
/// Precedence: env key > env passphrase-file > interactive unlock (verified) >
/// legacy machine key (only when no verifier is present).
///
/// `consult_passphrase_file` is `false` during rekey unlock: there the
/// passphrase file holds the *new* passphrase, so it must not be treated as the
/// current (old) key — the operator unlocks via env key, prompt, or legacy key.
fn resolve_master_for_read(
    cf: &CredentialFile,
    prompter: &dyn PassphrasePrompter,
    consult_passphrase_file: bool,
) -> Result<ResolvedMaster> {
    if let Ok(key) = std::env::var(ENV_MASTER_KEY)
        && !key.is_empty()
    {
        return Ok(ResolvedMaster { key });
    }

    if let Ok(path) = std::env::var(ENV_PASSPHRASE_FILE)
        && !path.is_empty()
        && consult_passphrase_file
    {
        let pass = read_passphrase_file(&path)?;
        if let Some(verifier) = &cf.verify
            && !verify_passphrase(&pass, verifier)?
        {
            return Err(StoreError::Decryption(format!(
                "passphrase in {ENV_PASSPHRASE_FILE} does not match the stored verifier"
            )));
        }
        return Ok(ResolvedMaster { key: pass });
    }

    // A verifier present means this file was created with a real passphrase.
    // Prompt interactively and verify.
    if let Some(verifier) = &cf.verify {
        if !prompter.is_interactive() {
            return Err(StoreError::Config(non_interactive_remediation()));
        }
        let pass = prompter.prompt_unlock()?;
        if !verify_passphrase(&pass, verifier)? {
            return Err(StoreError::Decryption(
                "incorrect passphrase (did not match the stored verifier)".into(),
            ));
        }
        return Ok(ResolvedMaster { key: pass });
    }

    // No verifier: legacy file created under the machine key. Read it, but warn.
    if cf.entries.contains_key(MASTER_KEY_ENTRY) {
        eprintln!(
            "warning: this credential store uses the legacy machine-derived key \
             (breaks if hostname/username change). Run `envelope accounts rekey` \
             to migrate it to a passphrase."
        );
        return Ok(ResolvedMaster {
            key: machine_master_key()?,
        });
    }

    // Fresh file, no verifier, no entries, no env: nothing to read.
    Err(StoreError::Config(
        "credential file does not contain an Envelope master key".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Get or create the master passphrase using the default (non-interactive)
/// prompter. Suitable for automation contexts where env vars must be set.
pub fn get_or_create_passphrase(backend: CredentialBackend) -> Result<String> {
    get_or_create_passphrase_with(backend, &NonInteractive)
}

/// Get or create the master (DB) passphrase, using the supplied prompter for
/// interactive first-time setup / unlock on the file backend.
pub fn get_or_create_passphrase_with(
    backend: CredentialBackend,
    prompter: &dyn PassphrasePrompter,
) -> Result<String> {
    match backend {
        CredentialBackend::File => file_get_or_create_passphrase(prompter, false),
        CredentialBackend::Keychain => keychain_get_or_create_passphrase(),
    }
}

/// Get or create the master (DB) passphrase, permitting the legacy
/// machine-derived key to be *established* for a brand-new file. Only invoked
/// behind the explicit `--insecure-machine-key` opt-in.
pub fn get_or_create_passphrase_insecure_machine(backend: CredentialBackend) -> Result<String> {
    match backend {
        CredentialBackend::File => file_get_or_create_passphrase(&NonInteractive, true),
        CredentialBackend::Keychain => keychain_get_or_create_passphrase(),
    }
}

/// Read the existing master passphrase without creating or mutating credential
/// storage. Uses a non-interactive prompter, so a passphrase-protected file
/// without env vars fails loud rather than prompting.
pub fn get_passphrase(backend: CredentialBackend) -> Result<String> {
    get_passphrase_with(backend, &NonInteractive)
}

/// Read the existing master passphrase (non-mutating), using the supplied
/// prompter to unlock a passphrase-protected file if needed.
pub fn get_passphrase_with(
    backend: CredentialBackend,
    prompter: &dyn PassphrasePrompter,
) -> Result<String> {
    match backend {
        CredentialBackend::File => file_get_passphrase(prompter),
        CredentialBackend::Keychain => keychain_get_passphrase(),
    }
}

/// File backend: the DB passphrase is a random value wrapped by the master key
/// under [`MASTER_KEY_ENTRY`]. This returns the existing one, or establishes a
/// new one (first-time setup).
fn file_get_or_create_passphrase(
    prompter: &dyn PassphrasePrompter,
    allow_machine_create: bool,
) -> Result<String> {
    let mut cf = read_credential_file()?;

    // Existing store: unlock and return the wrapped DB passphrase.
    if cf.entries.contains_key(MASTER_KEY_ENTRY) {
        let resolved = resolve_master_for_read(&cf, prompter, true)?;
        let encrypted = cf.entries.get(MASTER_KEY_ENTRY).unwrap();
        return decrypt_value(encrypted, &resolved.key);
    }

    // First-time setup: pick a master key.
    // Env var / passphrase-file take precedence; otherwise prompt to establish
    // one; machine key only under explicit opt-in.
    let (master, verifier): (String, Option<String>) =
        if let Ok(key) = std::env::var(ENV_MASTER_KEY) {
            if !key.is_empty() {
                (key, None)
            } else {
                first_time_from_prompt_or_machine(prompter, allow_machine_create)?
            }
        } else if let Ok(path) = std::env::var(ENV_PASSPHRASE_FILE) {
            if !path.is_empty() {
                let pass = read_passphrase_file(&path)?;
                let verifier = make_verifier(&pass)?;
                (pass, Some(verifier))
            } else {
                first_time_from_prompt_or_machine(prompter, allow_machine_create)?
            }
        } else {
            first_time_from_prompt_or_machine(prompter, allow_machine_create)?
        };

    // Generate the random DB passphrase, wrap it under the master key, persist.
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let passphrase = B64.encode(bytes);

    let encrypted = encrypt_value(&passphrase, &master)?;
    cf.entries.insert(MASTER_KEY_ENTRY.to_string(), encrypted);
    cf.verify = verifier;
    write_credential_file(&cf)?;

    Ok(passphrase)
}

/// First-time master-key selection when no env var is set: prompt for a new
/// passphrase if interactive, otherwise fall back to the machine key only when
/// explicitly permitted; else fail loud.
fn first_time_from_prompt_or_machine(
    prompter: &dyn PassphrasePrompter,
    allow_machine_create: bool,
) -> Result<(String, Option<String>)> {
    if prompter.is_interactive() {
        let pass = prompter.prompt_new()?;
        let verifier = make_verifier(&pass)?;
        return Ok((pass, Some(verifier)));
    }
    if allow_machine_create {
        eprintln!(
            "warning: creating credential store with the INSECURE machine-derived key. \
             It breaks if hostname/username change. Prefer a passphrase: set \
             {ENV_PASSPHRASE_FILE} or run interactively."
        );
        return Ok((machine_master_key()?, None));
    }
    Err(StoreError::Config(non_interactive_remediation()))
}

fn file_get_passphrase(prompter: &dyn PassphrasePrompter) -> Result<String> {
    let cf = read_credential_file()?;
    if !cf.entries.contains_key(MASTER_KEY_ENTRY) {
        return Err(StoreError::Config(
            "credential file does not contain an Envelope master key".to_string(),
        ));
    }
    let resolved = resolve_master_for_read(&cf, prompter, true)?;
    let encrypted = cf.entries.get(MASTER_KEY_ENTRY).unwrap();
    decrypt_value(encrypted, &resolved.key)
}

/// Outcome of a rekey operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RekeyOutcome {
    /// Store re-wrapped under a new passphrase.
    Rekeyed,
    /// No credential store existed to rekey.
    Nothing,
}

/// Re-wrap the credential store under a new passphrase.
///
/// Unlocks with the current key source — `ENVELOPE_MASTER_KEY`, an interactive
/// prompt (verified against the stored verifier), or the legacy machine key —
/// then obtains the *new* passphrase from `ENVELOPE_MASTER_PASSPHRASE_FILE` if
/// set, else via `new_prompter`. Because the passphrase file supplies the new
/// passphrase, it is deliberately *not* consulted during unlock; migrating a
/// legacy machine-key store therefore just works (unlock falls through to the
/// machine key). Re-encrypts the wrapped DB passphrase under the new key, writes
/// an Argon2 verifier, and persists atomically. The DB passphrase itself is
/// unchanged, so account rows never need re-encryption.
pub fn rekey(
    backend: CredentialBackend,
    unlock_prompter: &dyn PassphrasePrompter,
    new_prompter: &dyn PassphrasePrompter,
) -> Result<RekeyOutcome> {
    if backend != CredentialBackend::File {
        return Err(StoreError::Config(
            "rekey is only supported for the file credential backend".into(),
        ));
    }

    let mut cf = read_credential_file()?;
    if !cf.entries.contains_key(MASTER_KEY_ENTRY) {
        return Ok(RekeyOutcome::Nothing);
    }

    // 1. Unlock with the current key source and recover the DB passphrase.
    let current = resolve_master_for_read(&cf, unlock_prompter, false)?;
    let db_passphrase = decrypt_value(cf.entries.get(MASTER_KEY_ENTRY).unwrap(), &current.key)?;

    // 2. Obtain the new passphrase.
    let new_pass = if let Ok(path) = std::env::var(ENV_PASSPHRASE_FILE) {
        if path.is_empty() {
            new_prompter.prompt_new()?
        } else {
            read_passphrase_file(&path)?
        }
    } else {
        new_prompter.prompt_new()?
    };

    // 3. Re-wrap the DB passphrase under the new key and compute a verifier.
    let encrypted = encrypt_value(&db_passphrase, &new_pass)?;
    let verifier = make_verifier(&new_pass)?;
    cf.entries.insert(MASTER_KEY_ENTRY.to_string(), encrypted);
    cf.verify = Some(verifier);

    // 4. Persist atomically (temp + rename); the old file is untouched on error.
    write_credential_file_atomic(&cf)?;

    Ok(RekeyOutcome::Rekeyed)
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

fn keychain_get_passphrase() -> Result<String> {
    #[cfg(feature = "keychain")]
    {
        let entry = keyring::Entry::new(SERVICE_NAME, MASTER_KEY_ENTRY)
            .map_err(|e| StoreError::Keyring(e.to_string()))?;
        entry
            .get_password()
            .map_err(|e| StoreError::Keyring(e.to_string()))
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
                // Store it in the file backend under the machine key (legacy).
                let master = machine_master_key()?;
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
    use std::sync::{Mutex, MutexGuard};

    // Env vars + the shared credential-file path are process-global. Serialize
    // every file-backend test through one mutex so they never race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TestEnv {
        _guard: MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
    }

    /// Point the credential store at a fresh temp HOME and clear the relevant
    /// env vars. Holds the global lock for the duration.
    fn test_env() -> TestEnv {
        let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        // SAFETY: single lock holder at a time (ENV_LOCK), so no data race.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", home.path());
            std::env::set_var("HOME", home.path());
            std::env::remove_var(ENV_MASTER_KEY);
            std::env::remove_var(ENV_PASSPHRASE_FILE);
        }
        TestEnv {
            _guard: guard,
            _home: home,
        }
    }

    /// Prompter that yields fixed values for interactive tests.
    struct FakePrompter {
        interactive: bool,
        unlock: String,
        new: String,
    }
    impl FakePrompter {
        fn interactive(unlock: &str, new: &str) -> Self {
            Self {
                interactive: true,
                unlock: unlock.to_string(),
                new: new.to_string(),
            }
        }
    }
    impl PassphrasePrompter for FakePrompter {
        fn is_interactive(&self) -> bool {
            self.interactive
        }
        fn prompt_unlock(&self) -> Result<String> {
            Ok(self.unlock.clone())
        }
        fn prompt_new(&self) -> Result<String> {
            Ok(self.new.clone())
        }
    }

    fn set_env(key: &str, val: &str) {
        // SAFETY: called only while holding ENV_LOCK via TestEnv.
        unsafe { std::env::set_var(key, val) }
    }

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

    #[test]
    fn env_master_key_takes_precedence_and_is_stable() {
        let _env = test_env();
        set_env(ENV_MASTER_KEY, "explicit-master-key-abc");
        let p1 = get_or_create_passphrase(CredentialBackend::File).unwrap();
        let p2 = get_or_create_passphrase(CredentialBackend::File).unwrap();
        assert_eq!(p1, p2);
        // No verifier is stored when using the raw env master key.
        let cf = read_credential_file().unwrap();
        assert!(cf.verify.is_none());
    }

    #[test]
    fn passphrase_file_loading_strips_newline_and_creates_verifier() {
        let _env = test_env();
        let dir = tempfile::tempdir().unwrap();
        let pf = dir.path().join("pass");
        std::fs::write(&pf, "hunter2\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&pf, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        set_env(ENV_PASSPHRASE_FILE, pf.to_str().unwrap());

        let p1 = get_or_create_passphrase(CredentialBackend::File).unwrap();
        // A verifier is written for passphrase-file setup.
        let cf = read_credential_file().unwrap();
        assert!(cf.verify.is_some());
        // The trailing newline was stripped: the exact same passphrase unlocks.
        let p2 = get_passphrase(CredentialBackend::File).unwrap();
        assert_eq!(p1, p2);
        // The verifier matches "hunter2", not "hunter2\n".
        assert!(verify_passphrase("hunter2", cf.verify.as_ref().unwrap()).unwrap());
        assert!(!verify_passphrase("hunter2\n", cf.verify.as_ref().unwrap()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn passphrase_file_world_readable_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let _env = test_env();
        let dir = tempfile::tempdir().unwrap();
        let pf = dir.path().join("pass");
        std::fs::write(&pf, "secret\n").unwrap();
        std::fs::set_permissions(&pf, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_passphrase_file(pf.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("chmod 600"));
    }

    #[test]
    fn first_time_interactive_establishes_verifier_and_wrong_passphrase_rejected() {
        let _env = test_env();
        let prompter = FakePrompter::interactive("unused", "setup-pass");
        let p1 = get_or_create_passphrase_with(CredentialBackend::File, &prompter).unwrap();

        let cf = read_credential_file().unwrap();
        assert!(cf.verify.is_some());

        // Correct passphrase unlocks and returns the same DB passphrase.
        let good = FakePrompter::interactive("setup-pass", "unused");
        let p2 = get_passphrase_with(CredentialBackend::File, &good).unwrap();
        assert_eq!(p1, p2);

        // Wrong passphrase is rejected by the verifier.
        let bad = FakePrompter::interactive("wrong-pass", "unused");
        let err = get_passphrase_with(CredentialBackend::File, &bad).unwrap_err();
        assert!(err.to_string().contains("passphrase"));
    }

    #[test]
    fn non_tty_without_env_fails_loud_on_first_setup() {
        let _env = test_env();
        let err = get_or_create_passphrase(CredentialBackend::File).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(ENV_PASSPHRASE_FILE));
        assert!(msg.contains("--insecure-machine-key"));
    }

    #[test]
    fn legacy_machine_key_file_still_decrypts_without_verifier() {
        let _env = test_env();
        // Simulate a legacy file: DB passphrase wrapped under the machine key,
        // no verifier field present.
        let master = machine_master_key().unwrap();
        let db_pass = "legacy-db-passphrase";
        let mut cf = CredentialFile::default();
        cf.entries.insert(
            MASTER_KEY_ENTRY.to_string(),
            encrypt_value(db_pass, &master).unwrap(),
        );
        cf.verify = None;
        write_credential_file(&cf).unwrap();

        // A legacy file with no env vars still reads via the machine key.
        let got = get_passphrase(CredentialBackend::File).unwrap();
        assert_eq!(got, db_pass);
    }

    #[test]
    fn insecure_machine_key_create_writes_no_verifier() {
        let _env = test_env();
        let p1 = get_or_create_passphrase_insecure_machine(CredentialBackend::File).unwrap();
        let cf = read_credential_file().unwrap();
        assert!(cf.verify.is_none());
        let p2 = get_or_create_passphrase_insecure_machine(CredentialBackend::File).unwrap();
        assert_eq!(p1, p2);
    }

    #[test]
    fn rekey_roundtrip_old_fails_new_works() {
        let _env = test_env();
        // Establish an initial store under passphrase "old-pass".
        let init = FakePrompter::interactive("unused", "old-pass");
        let db_pass = get_or_create_passphrase_with(CredentialBackend::File, &init).unwrap();

        // Rekey: unlock with "old-pass", set new "new-pass".
        let unlock = FakePrompter::interactive("old-pass", "unused");
        let newp = FakePrompter::interactive("unused", "new-pass");
        let outcome = rekey(CredentialBackend::File, &unlock, &newp).unwrap();
        assert_eq!(outcome, RekeyOutcome::Rekeyed);

        // Old passphrase no longer unlocks.
        let old = FakePrompter::interactive("old-pass", "unused");
        assert!(get_passphrase_with(CredentialBackend::File, &old).is_err());

        // New passphrase unlocks and yields the SAME DB passphrase (accounts
        // never need re-encryption).
        let new_ok = FakePrompter::interactive("new-pass", "unused");
        let got = get_passphrase_with(CredentialBackend::File, &new_ok).unwrap();
        assert_eq!(got, db_pass);
    }

    #[test]
    fn rekey_nothing_when_no_store() {
        let _env = test_env();
        let unlock = FakePrompter::interactive("x", "x");
        let newp = FakePrompter::interactive("x", "x");
        let outcome = rekey(CredentialBackend::File, &unlock, &newp).unwrap();
        assert_eq!(outcome, RekeyOutcome::Nothing);
    }

    #[test]
    fn rekey_takes_new_passphrase_from_file_and_ignores_it_for_unlock() {
        let _env = test_env();
        // Establish under an env master key, then rekey to a passphrase file.
        set_env(ENV_MASTER_KEY, "old-env-master");
        let db_pass = get_or_create_passphrase(CredentialBackend::File).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let pf = dir.path().join("newpass");
        std::fs::write(&pf, "file-new-pass\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&pf, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        set_env(ENV_PASSPHRASE_FILE, pf.to_str().unwrap());

        // Unlock still uses ENV_MASTER_KEY (the file supplies only the new pass).
        let outcome = rekey(CredentialBackend::File, &NonInteractive, &NonInteractive).unwrap();
        assert_eq!(outcome, RekeyOutcome::Rekeyed);

        // After rekey the env master key must NOT unlock; the file passphrase must.
        // SAFETY: holding ENV_LOCK via _env.
        unsafe { std::env::remove_var(ENV_MASTER_KEY) }
        let got = get_passphrase(CredentialBackend::File).unwrap();
        assert_eq!(got, db_pass);
        let cf = read_credential_file().unwrap();
        assert!(verify_passphrase("file-new-pass", cf.verify.as_ref().unwrap()).unwrap());
    }

    #[test]
    fn rekey_migrates_legacy_machine_key_to_passphrase() {
        let _env = test_env();
        // Legacy file: wrapped under machine key, no verifier.
        let master = machine_master_key().unwrap();
        let db_pass = "legacy-db-pass";
        let mut cf = CredentialFile::default();
        cf.entries.insert(
            MASTER_KEY_ENTRY.to_string(),
            encrypt_value(db_pass, &master).unwrap(),
        );
        write_credential_file(&cf).unwrap();

        // Rekey: unlock falls through to the machine key (no env, no verifier),
        // new passphrase set interactively.
        let newp = FakePrompter::interactive("unused", "migrated-pass");
        let outcome = rekey(CredentialBackend::File, &NonInteractive, &newp).unwrap();
        assert_eq!(outcome, RekeyOutcome::Rekeyed);

        // Now a verifier exists and the new passphrase unlocks the same DB pass.
        let after = read_credential_file().unwrap();
        assert!(after.verify.is_some());
        let ok = FakePrompter::interactive("migrated-pass", "unused");
        assert_eq!(
            get_passphrase_with(CredentialBackend::File, &ok).unwrap(),
            db_pass
        );
    }

    #[test]
    fn atomic_write_leaves_original_on_failure() {
        let _env = test_env();
        // Establish a store.
        set_env(ENV_MASTER_KEY, "km");
        let original = get_or_create_passphrase(CredentialBackend::File).unwrap();
        let before = std::fs::read_to_string(paths::credential_file_path()).unwrap();

        // Force the atomic write to fail by making the parent dir's temp target
        // impossible: create a directory where the temp file would go.
        let path = paths::credential_file_path();
        let mut tmp = path.clone();
        let pid = std::process::id();
        tmp.set_file_name(format!(".credentials.json.tmp.{pid}"));
        std::fs::create_dir(&tmp).unwrap(); // now File::create(tmp) fails

        let cf = read_credential_file().unwrap();
        let err = write_credential_file_atomic(&cf).unwrap_err();
        assert!(err.to_string().contains("temp"));

        // Original file is byte-identical and still decrypts.
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
        let got = get_passphrase(CredentialBackend::File).unwrap();
        assert_eq!(got, original);

        std::fs::remove_dir(&tmp).ok();
    }
}
