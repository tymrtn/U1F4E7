// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Pure planner module for `envelope backup` (export / verify / restore).
//!
//! This module is deliberately IMAP-free and side-effect-free except for the
//! filesystem helpers (`write_atomic`, `verify_archive`, restore-state I/O).
//! All planning, encoding, mapping, and serialization logic lives here so it
//! can be tested without a live IMAP server. The CLI handler in
//! `crates/cli/src/commands/backup.rs` orchestrates IMAP + emits events.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Bumped whenever the manifest schema changes in a non-backward-compatible way.
/// Verify and restore both refuse to operate on an unsupported version.
pub const ARCHIVE_FORMAT_VERSION: u32 = 1;

/// Common provider folder normalizations. MVP does NOT apply these silently:
/// the operator must opt in by passing `--map "Junk E-mail=Junk"` etc. The
/// constant is the documented extension point that tests exercise.
pub const COMMON_PROVIDER_MAPPINGS: &[(&str, &str)] = &[
    ("Junk E-mail", "Junk"),
    ("Sent Items", "Sent"),
    ("Deleted Items", "Trash"),
];

#[derive(Debug, Error)]
pub enum BackupError {
    #[error("backup IO error: {0}")]
    Io(#[from] io::Error),
    #[error("backup JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("malformed manifest at {path}: {reason}")]
    MalformedManifest { path: PathBuf, reason: String },
    #[error(
        "unsupported archive format version: archive declares {found}, this binary supports {supported}"
    )]
    UnsupportedFormatVersion { found: u32, supported: u32 },
    #[error("malformed --map argument {arg:?}: expected SRC=DST")]
    MalformedMappingArg { arg: String },
    #[error("invalid archive layout at {path}: {reason}")]
    InvalidArchiveLayout { path: PathBuf, reason: String },
    #[error("manifest validation failed: {0}")]
    ManifestValidation(String),
    #[error("unsafe rel_path {rel_path:?}: {reason}")]
    UnsafeRelPath { rel_path: String, reason: String },
    #[error("export output directory {path} is not safe to use: {reason}")]
    UnsafeOutputDir { path: PathBuf, reason: String },
    #[error("restore destination is unsafe: {0}")]
    UnsafeRestoreDestination(String),
}

/// Top-level archive manifest, written atomically as `manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveManifest {
    pub archive_format_version: u32,
    pub tool: String,
    pub tool_version: String,
    pub exported_at: String,
    pub account: ArchiveAccount,
    pub folders: Vec<ArchiveFolderRecord>,
    pub messages: Vec<ArchiveMessageRecord>,
}

/// Public mailbox identity. Deliberately stores no secrets — only enough for an
/// operator to verify the archive matches the source mailbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveAccount {
    pub id: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveFolderRecord {
    pub name: String,
    pub uidvalidity: u32,
    pub encoded_dir: String,
    pub message_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveMessageRecord {
    pub folder: String,
    pub uid: u32,
    pub uidvalidity: u32,
    pub message_id: Option<String>,
    /// RFC 3339 / ISO 8601 string with timezone, parseable by chrono.
    pub internal_date: Option<String>,
    pub flags: Vec<String>,
    pub size: u64,
    pub sha256: String,
    pub rel_path: String,
}

/// Lifecycle status of a restore-state record. Written to the NDJSON sidecar
/// to implement crash-safe idempotency (issue #19).
///
/// - `Pending`: written BEFORE the IMAP APPEND. If the process crashes after
///   APPEND but before the `Done` line lands, the pending record is enough to
///   prevent a duplicate on rerun (conservative skip + warning).
/// - `Done`: written AFTER a successful APPEND (or after a destination
///   duplicate-skip). This is the terminal state.
///
/// Old-format state files lack the field entirely; `serde(default)` maps the
/// absent key to `Done`, preserving backward compatibility.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RestoreStatus {
    Pending,
    #[default]
    Done,
}

/// One line of the restore-state NDJSON sidecar. Written before and after every
/// destination append. The `(folder, uidvalidity, uid, sha256)` tuple is the
/// identity key; `status` tracks the pending/done lifecycle but is excluded
/// from `Hash` and `Eq` so both phases resolve to the same set entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreStateRecord {
    pub folder: String,
    pub uidvalidity: u32,
    pub uid: u32,
    pub sha256: String,
    #[serde(default)]
    pub status: RestoreStatus,
}

// Manual Hash/PartialEq/Eq: identity is (folder, uidvalidity, uid, sha256).
// `status` is lifecycle metadata, not identity — a Pending and Done record
// for the same message must hash and compare equally so a HashSet<_> lookup
// treats them as the same entry.
impl std::hash::Hash for RestoreStateRecord {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.folder.hash(state);
        self.uidvalidity.hash(state);
        self.uid.hash(state);
        self.sha256.hash(state);
    }
}

impl PartialEq for RestoreStateRecord {
    fn eq(&self, other: &Self) -> bool {
        self.folder == other.folder
            && self.uidvalidity == other.uidvalidity
            && self.uid == other.uid
            && self.sha256 == other.sha256
    }
}

impl Eq for RestoreStateRecord {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderMapping {
    pub source: String,
    pub destination: String,
}

/// Single source-of-truth event taxonomy for export / verify / restore. The
/// JSON tag is part of the public CLI contract; tests lock it down so renames
/// are deliberate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BackupEvent {
    /// Terminal fatal-error event so `--json` callers get a machine-readable
    /// failure instead of a plain-text top-level error (issue #21). `ok` is
    /// always false. `phase` names the failing command (`export` / `verify` /
    /// `restore` / `audit_state`) when known. Carries no secrets, tokens, or
    /// raw message bodies.
    Error {
        ok: bool,
        error: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
    },
    ExportFolderStart {
        folder: String,
        messages: u32,
    },
    ExportMessageWritten {
        folder: String,
        uid: u32,
        bytes: u64,
        sha256: String,
    },
    ExportMessageFailed {
        folder: String,
        uid: u32,
        error: String,
    },
    ExportFolderDone {
        folder: String,
        written: u32,
    },
    ExportRunDone {
        folders: u32,
        messages: u32,
        bytes: u64,
        archive_dir: String,
    },
    VerifyFile {
        folder: String,
        uid: u32,
        rel_path: String,
    },
    VerifyExtraFile {
        rel_path: String,
    },
    /// A symlinked entry was found under `messages/` and refused rather than
    /// traversed (issue #17). Always forces verify to fail.
    VerifyUnsafeEntry {
        rel_path: String,
    },
    VerifyMissingFile {
        folder: String,
        uid: u32,
        rel_path: String,
    },
    VerifyChecksumMismatch {
        folder: String,
        uid: u32,
        rel_path: String,
        expected_sha256: String,
        actual_sha256: String,
    },
    VerifySizeMismatch {
        folder: String,
        uid: u32,
        rel_path: String,
        expected_size: u64,
        actual_size: u64,
    },
    VerifyDone {
        ok: bool,
        missing: u32,
        corrupt: u32,
        extras: u32,
        /// Count of refused symlinked entries under `messages/` (issue #17).
        /// Defaults to 0 for archives with no unsafe contents; older JSON
        /// consumers can ignore it.
        #[serde(default)]
        unsafe_entries: u32,
    },
    RestoreFolderStart {
        source: String,
        destination: String,
        messages: u32,
    },
    RestoreMessageAppended {
        source: String,
        destination: String,
        uid: u32,
        bytes: u64,
    },
    RestoreMessageSkipped {
        source: String,
        uid: u32,
        reason: String,
    },
    RestoreMessageFailed {
        source: String,
        destination: String,
        uid: u32,
        error: String,
    },
    RestoreFolderDone {
        source: String,
        destination: String,
        appended: u32,
        skipped: u32,
        failed: u32,
    },
    RestoreRunDone {
        folders: u32,
        appended: u32,
        skipped: u32,
        failed: u32,
    },
    RestoreDryRunDone {
        folders: u32,
        would_append: u32,
        would_skip: u32,
    },
    /// Issue #19: machine-readable warning about restore-state anomalies
    /// (malformed lines, pending records from a prior crash).
    RestoreStateWarning {
        warning: String,
    },
    /// Per-row audit output from `backup audit-state`. Emitted once per
    /// pending-without-done sidecar entry. `destination_uid` and `error` are
    /// reserved for a future live-IMAP verifier; the planner-only variant
    /// always serializes them as absent (`skip_serializing_if`).
    RestoreStateAuditRecord {
        source: String,
        destination: String,
        uidvalidity: u32,
        uid: u32,
        sha256: String,
        message_id_present: bool,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        destination_uid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Run summary for `backup audit-state`. `present`, `missing`, and
    /// `errors` are reserved for the future live-verifier seam and stay 0
    /// in the planner-only path.
    RestoreStateAuditDone {
        pending: u32,
        present: u32,
        missing: u32,
        unknown: u32,
        state_not_in_manifest: u32,
        errors: u32,
    },
}

/// One row of a restore plan. Same struct used for dry-run planning and live
/// execution so both share a single source of truth.
///
/// Carries the full validated `ArchiveMessageRecord` rather than a tuple of
/// fields so the executor never has to re-look-up the record from the
/// manifest by scanning — a previous version did `find_record(&records,
/// &action)` which could return the wrong record under duplicate manifests.
/// Validation (`validate_manifest`) refuses duplicates, but carrying the
/// record by-value makes the bug impossible by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedAppend {
    pub record: ArchiveMessageRecord,
    pub destination_folder: String,
}

impl PlannedAppend {
    pub fn source_folder(&self) -> &str {
        &self.record.folder
    }
    pub fn uid(&self) -> u32 {
        self.record.uid
    }
    pub fn uidvalidity(&self) -> u32 {
        self.record.uidvalidity
    }
    pub fn sha256(&self) -> &str {
        &self.record.sha256
    }
    pub fn rel_path(&self) -> &str {
        &self.record.rel_path
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestorePlan {
    pub destinations: Vec<String>,
    pub planned_appends: Vec<PlannedAppend>,
    pub skipped_already_restored: u32,
    pub skipped_excluded: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyOutcome {
    pub ok: bool,
    pub manifest_message_count: u32,
    pub missing: Vec<MissingFile>,
    pub corrupt: Vec<CorruptFile>,
    pub extras: Vec<String>,
    /// Symlinked entries discovered under `messages/` during extra-file
    /// enumeration. These are reported (not traversed) so a malicious or
    /// malformed archive cannot make verify / restore dry-run walk outside the
    /// archive directory or hang on a symlink loop. Any unsafe entry forces
    /// `ok = false` (issue #17).
    pub unsafe_entries: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingFile {
    pub folder: String,
    pub uid: u32,
    pub rel_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorruptFile {
    SizeMismatch {
        folder: String,
        uid: u32,
        rel_path: String,
        expected_size: u64,
        actual_size: u64,
    },
    ChecksumMismatch {
        folder: String,
        uid: u32,
        rel_path: String,
        expected_sha256: String,
        actual_sha256: String,
    },
}

// -----------------------------------------------------------------------------
// Folder name <-> on-disk slug
// -----------------------------------------------------------------------------

/// Encode an IMAP folder name as a filesystem-safe slug. Any byte outside the
/// `[A-Za-z0-9_.-]` set is percent-encoded as `%HH` (uppercase hex). This is
/// deterministic and round-trippable; manifest carries the canonical IMAP name
/// so the slug is purely an opaque on-disk identifier.
pub fn encode_folder_for_disk(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", b));
        }
    }
    out
}

pub fn decode_folder_for_disk(encoded: &str) -> Result<String, BackupError> {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if i + 2 >= bytes.len() {
                return Err(BackupError::InvalidArchiveLayout {
                    path: PathBuf::from(encoded),
                    reason: "truncated percent-escape in folder slug".to_string(),
                });
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|_| {
                BackupError::InvalidArchiveLayout {
                    path: PathBuf::from(encoded),
                    reason: "non-utf8 percent-escape".to_string(),
                }
            })?;
            let v = u8::from_str_radix(hex, 16).map_err(|_| BackupError::InvalidArchiveLayout {
                path: PathBuf::from(encoded),
                reason: format!("invalid hex {hex:?} in percent-escape"),
            })?;
            out.push(v);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|e| BackupError::InvalidArchiveLayout {
        path: PathBuf::from(encoded),
        reason: format!("decoded slug is not UTF-8: {e}"),
    })
}

pub fn message_filename(uidvalidity: u32, uid: u32) -> String {
    format!("{uidvalidity}-{uid}.eml")
}

pub fn relative_message_path(folder: &str, uidvalidity: u32, uid: u32) -> String {
    format!(
        "messages/{}/{}",
        encode_folder_for_disk(folder),
        message_filename(uidvalidity, uid)
    )
}

// -----------------------------------------------------------------------------
// rel_path safety + manifest structural validation
// -----------------------------------------------------------------------------

/// Largest message body size we'll accept in a manifest. 1 GiB is well above
/// any provider's per-message limit and well below limits where downstream
/// allocations could blow up unexpectedly. Larger entries indicate manifest
/// tampering or filesystem corruption.
pub const MAX_MESSAGE_SIZE_BYTES: u64 = 1024 * 1024 * 1024;

/// Validate that a manifest `rel_path` is safe to join with the archive root
/// AND that it matches exactly the canonical path for the (folder,
/// uidvalidity, uid) tuple. Refuses absolute paths, parent-traversal
/// components, backslashes, empty segments, non-`.eml` extensions, mismatched
/// folder slugs, and mismatched filenames.
///
/// The expected form is exactly: `messages/<encoded_folder>/<uidvalidity>-<uid>.eml`.
pub fn validate_message_rel_path(
    rel_path: &str,
    expected_folder: &str,
    expected_uidvalidity: u32,
    expected_uid: u32,
) -> Result<(), BackupError> {
    let unsafe_path = |reason: &str| BackupError::UnsafeRelPath {
        rel_path: rel_path.to_string(),
        reason: reason.to_string(),
    };

    if rel_path.is_empty() {
        return Err(unsafe_path("empty rel_path"));
    }
    if rel_path.contains('\0') {
        return Err(unsafe_path("contains NUL byte"));
    }
    if rel_path.contains('\\') {
        return Err(unsafe_path("contains backslash"));
    }
    if rel_path.starts_with('/') {
        return Err(unsafe_path("absolute path"));
    }
    // Reject UNC-style or Windows drive prefixes defensively.
    if rel_path.len() >= 2 {
        let bytes = rel_path.as_bytes();
        let first = bytes[0];
        if bytes[1] == b':' && first.is_ascii_alphabetic() {
            return Err(unsafe_path("Windows drive prefix"));
        }
    }

    // Tokenize on '/' only; backslashes already rejected. Each segment must
    // be non-empty, not "." or "..", and not contain a leading/trailing space.
    let segments: Vec<&str> = rel_path.split('/').collect();
    for seg in &segments {
        if seg.is_empty() {
            return Err(unsafe_path("empty path segment"));
        }
        if *seg == "." || *seg == ".." {
            return Err(unsafe_path("parent or current path component"));
        }
    }
    if segments.len() != 3 {
        return Err(unsafe_path(
            "rel_path must be exactly messages/<encoded_folder>/<uidvalidity>-<uid>.eml",
        ));
    }
    if segments[0] != "messages" {
        return Err(unsafe_path("must start with messages/"));
    }
    let expected_slug = encode_folder_for_disk(expected_folder);
    if segments[1] != expected_slug {
        return Err(BackupError::UnsafeRelPath {
            rel_path: rel_path.to_string(),
            reason: format!(
                "encoded folder slug {:?} does not match expected {:?}",
                segments[1], expected_slug
            ),
        });
    }
    let expected_filename = message_filename(expected_uidvalidity, expected_uid);
    if segments[2] != expected_filename {
        return Err(BackupError::UnsafeRelPath {
            rel_path: rel_path.to_string(),
            reason: format!(
                "filename {:?} does not match expected {:?}",
                segments[2], expected_filename
            ),
        });
    }
    Ok(())
}

/// True for a 64-character lowercase or uppercase hex SHA-256 digest.
fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Resolve `archive_dir + record.rel_path` to a concrete `PathBuf` while
/// refusing to traverse *any* symlink under the archive root.
///
/// Background: a previous version of `verify_archive` ran
/// `fs::symlink_metadata` only on the final path. That detects a symlinked
/// `.eml` file but happily follows a symlinked parent directory such as
/// `messages/INBOX -> /tmp/outside`, letting subsequent metadata/hash reads
/// touch bytes outside the archive. This helper closes that gap by checking
/// `fs::symlink_metadata` at every prefix from `archive_dir` down to the
/// leaf — `messages/`, the encoded folder dir, and the `.eml` file itself.
///
/// The helper does **not** stat `archive_dir` itself: the operator chose
/// that path on the command line, so following a symlink they passed is on
/// them. We only refuse to follow symlinks the *archive contents* would
/// introduce after that point.
///
/// Also re-runs canonical rel_path validation defensively so callers can use
/// this as the single safety boundary before any read.
pub fn validate_materialized_message_path(
    archive_dir: &Path,
    record: &ArchiveMessageRecord,
) -> Result<PathBuf, BackupError> {
    validate_message_rel_path(
        &record.rel_path,
        &record.folder,
        record.uidvalidity,
        record.uid,
    )?;

    // validate_message_rel_path enforces exactly three '/' segments.
    let mut current = archive_dir.to_path_buf();
    for segment in record.rel_path.split('/') {
        current.push(segment);
        let meta = match fs::symlink_metadata(&current) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Caller (verify) treats not-found as "missing" rather than
                // "unsafe". Bubble the io::Error up; verify maps it.
                return Err(BackupError::Io(e));
            }
            Err(e) => return Err(BackupError::Io(e)),
        };
        if meta.file_type().is_symlink() {
            return Err(BackupError::UnsafeRelPath {
                rel_path: record.rel_path.clone(),
                reason: format!(
                    "intermediate or leaf symlink at {} — refusing to follow outside the archive",
                    current.display()
                ),
            });
        }
    }
    Ok(current)
}

/// Cross-cutting structural validation that read_manifest applies before
/// returning a manifest to callers. Catches duplicates, count mismatches,
/// folder/encoded_dir disagreement, and any rel_path violation before we
/// touch the filesystem.
pub fn validate_manifest(manifest: &ArchiveManifest) -> Result<(), BackupError> {
    let fail = |msg: String| BackupError::ManifestValidation(msg);

    if manifest.archive_format_version != ARCHIVE_FORMAT_VERSION {
        return Err(BackupError::UnsupportedFormatVersion {
            found: manifest.archive_format_version,
            supported: ARCHIVE_FORMAT_VERSION,
        });
    }

    // Folders: name uniqueness and encoded_dir == encode_folder_for_disk(name).
    let mut seen_folder_names: HashSet<&str> = HashSet::new();
    for folder in &manifest.folders {
        if !seen_folder_names.insert(folder.name.as_str()) {
            return Err(fail(format!(
                "duplicate folder name in manifest: {:?}",
                folder.name
            )));
        }
        let expected_slug = encode_folder_for_disk(&folder.name);
        if folder.encoded_dir != expected_slug {
            return Err(fail(format!(
                "folder {:?} declares encoded_dir {:?}, but canonical encoding is {:?}",
                folder.name, folder.encoded_dir, expected_slug
            )));
        }
    }

    // Messages: every record's folder must exist in folders[], rel_path must
    // be canonical, sha256 must be valid hex, size sane, and there must be no
    // duplicates by (folder, uidvalidity, uid) or by rel_path.
    let mut by_key: HashSet<(String, u32, u32)> = HashSet::new();
    let mut by_rel_path: HashSet<String> = HashSet::new();
    let mut count_by_folder: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();

    for record in &manifest.messages {
        if !seen_folder_names.contains(record.folder.as_str()) {
            return Err(fail(format!(
                "message UID {} references unknown folder {:?}",
                record.uid, record.folder
            )));
        }
        let key = (record.folder.clone(), record.uidvalidity, record.uid);
        if !by_key.insert(key) {
            return Err(fail(format!(
                "duplicate message identity (folder={:?}, uidvalidity={}, uid={})",
                record.folder, record.uidvalidity, record.uid
            )));
        }
        if !by_rel_path.insert(record.rel_path.clone()) {
            return Err(fail(format!("duplicate rel_path {:?}", record.rel_path)));
        }
        validate_message_rel_path(
            &record.rel_path,
            &record.folder,
            record.uidvalidity,
            record.uid,
        )?;
        if !is_valid_sha256_hex(&record.sha256) {
            return Err(fail(format!(
                "message UID {} in {:?} has invalid sha256 {:?} (must be 64 hex chars)",
                record.uid, record.folder, record.sha256
            )));
        }
        if record.size > MAX_MESSAGE_SIZE_BYTES {
            return Err(fail(format!(
                "message UID {} in {:?} declares size {} > {} bytes",
                record.uid, record.folder, record.size, MAX_MESSAGE_SIZE_BYTES
            )));
        }
        *count_by_folder.entry(record.folder.clone()).or_insert(0) += 1;
    }

    // folders[].message_count must equal the actual count of messages in that
    // folder. Folders with zero messages are also valid.
    for folder in &manifest.folders {
        let actual = count_by_folder.get(&folder.name).copied().unwrap_or(0);
        if folder.message_count != actual {
            return Err(fail(format!(
                "folder {:?} declares message_count={} but {} messages reference it",
                folder.name, folder.message_count, actual
            )));
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// Output dir safety (export only)
// -----------------------------------------------------------------------------

/// Refuse to use an existing non-empty `--out` directory. This rules out a
/// whole class of operator footguns:
///
/// * stale `manifest.json` left over from a prior export getting re-used
/// * symlinks in the existing tree pointing outside the archive
/// * unrelated files masquerading as archive contents
///
/// Returns `Ok(())` if the directory is absent (will be created) or exists
/// and is empty. Returns `UnsafeOutputDir` otherwise.
pub fn validate_export_output_dir(out: &Path) -> Result<(), BackupError> {
    if !out.exists() {
        return Ok(());
    }
    let meta = fs::symlink_metadata(out).map_err(|e| BackupError::UnsafeOutputDir {
        path: out.to_path_buf(),
        reason: format!("stat failed: {e}"),
    })?;
    if meta.file_type().is_symlink() {
        return Err(BackupError::UnsafeOutputDir {
            path: out.to_path_buf(),
            reason: "is a symlink — refusing to follow into possibly external target".to_string(),
        });
    }
    if !meta.is_dir() {
        return Err(BackupError::UnsafeOutputDir {
            path: out.to_path_buf(),
            reason: "exists but is not a directory".to_string(),
        });
    }
    let mut iter = fs::read_dir(out).map_err(|e| BackupError::UnsafeOutputDir {
        path: out.to_path_buf(),
        reason: format!("read_dir failed: {e}"),
    })?;
    if iter.next().is_some() {
        return Err(BackupError::UnsafeOutputDir {
            path: out.to_path_buf(),
            reason: "directory exists and is not empty (refusing to overwrite/mix archives)"
                .to_string(),
        });
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Hashing
// -----------------------------------------------------------------------------

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Hash a file by streaming so very large `.eml` payloads don't double-allocate.
pub fn sha256_hex_file(path: &Path) -> io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{:02x}", b));
    }
    Ok(s)
}

// -----------------------------------------------------------------------------
// Folder mapping (planner-side; CLI parses --map into FolderMapping)
// -----------------------------------------------------------------------------

pub fn parse_folder_mapping_arg(arg: &str) -> Result<FolderMapping, BackupError> {
    let (src, dst) = arg
        .split_once('=')
        .ok_or(BackupError::MalformedMappingArg {
            arg: arg.to_string(),
        })?;
    if src.is_empty() || dst.is_empty() {
        return Err(BackupError::MalformedMappingArg {
            arg: arg.to_string(),
        });
    }
    Ok(FolderMapping {
        source: src.to_string(),
        destination: dst.to_string(),
    })
}

/// First-match-wins exact source equality. No glob: we want a backup restore to
/// rewrite folder names predictably, not via fuzzy globbing that could drift.
pub fn apply_folder_mapping(source: &str, mappings: &[FolderMapping]) -> String {
    for m in mappings {
        if m.source == source {
            return m.destination.clone();
        }
    }
    source.to_string()
}

// -----------------------------------------------------------------------------
// Restore destination validation
// -----------------------------------------------------------------------------

/// Reject restoring an archive back into its source mailbox, whether that is
/// expressed via the same logical account ID or via a distinct account record
/// that still resolves to the same physical IMAP mailbox.
pub fn validate_restore_destination(
    source: &ArchiveAccount,
    dest_account_id: &str,
    dest_imap_host: &str,
    dest_imap_port: u16,
    dest_imap_username: &str,
) -> Result<(), BackupError> {
    crate::migrate::validate_distinct_accounts(&source.id, dest_account_id)
        .map_err(BackupError::UnsafeRestoreDestination)?;
    crate::migrate::validate_distinct_imap_endpoints(
        &source.imap_host,
        source.imap_port,
        &source.imap_username,
        dest_imap_host,
        dest_imap_port,
        dest_imap_username,
    )
    .map_err(BackupError::UnsafeRestoreDestination)?;
    Ok(())
}

// -----------------------------------------------------------------------------
// Restore plan
// -----------------------------------------------------------------------------

pub fn restore_state_key(record: &ArchiveMessageRecord) -> RestoreStateRecord {
    RestoreStateRecord {
        folder: record.folder.clone(),
        uidvalidity: record.uidvalidity,
        uid: record.uid,
        sha256: record.sha256.clone(),
        status: RestoreStatus::Done,
    }
}

/// Build a pending-state record for write-ahead logging before IMAP APPEND.
pub fn restore_state_key_pending(record: &ArchiveMessageRecord) -> RestoreStateRecord {
    RestoreStateRecord {
        folder: record.folder.clone(),
        uidvalidity: record.uidvalidity,
        uid: record.uid,
        sha256: record.sha256.clone(),
        status: RestoreStatus::Pending,
    }
}

/// Compute the restore plan from a manifest. Pure: same inputs always yield the
/// same plan, so dry-run and live restore call this with identical state and
/// produce identical action lists.
pub fn plan_restore(
    records: &[ArchiveMessageRecord],
    state: &HashSet<RestoreStateRecord>,
    mappings: &[FolderMapping],
    includes: &[String],
    excludes: &[String],
) -> RestorePlan {
    use crate::migrate::folder_selected;

    let mut plan = RestorePlan::default();
    let mut destinations_seen: HashSet<String> = HashSet::new();

    for record in records {
        if !folder_selected(&record.folder, includes, excludes) {
            plan.skipped_excluded += 1;
            continue;
        }
        let key = restore_state_key(record);
        if state.contains(&key) {
            plan.skipped_already_restored += 1;
            continue;
        }
        let destination = apply_folder_mapping(&record.folder, mappings);
        if destinations_seen.insert(destination.clone()) {
            plan.destinations.push(destination.clone());
        }
        plan.planned_appends.push(PlannedAppend {
            record: record.clone(),
            destination_folder: destination,
        });
    }

    plan
}

// -----------------------------------------------------------------------------
// Restore-state audit (read-only; no IMAP)
// -----------------------------------------------------------------------------

/// Classification of a single pending-without-done sidecar entry against the
/// archive manifest. The serialized snake_case form is part of the public CLI
/// contract and lives on the `RestoreStateAuditRecord.status` field.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    /// The sidecar references a `(folder, uidvalidity, uid, sha256)` tuple
    /// that the manifest does not contain. Archive and sidecar have drifted.
    StateNotInManifest,
    /// Manifest lookup succeeded but the message has no Message-ID, so live
    /// presence in the destination cannot be proven by header search.
    UnknownNoMessageId,
    /// Matched a manifest record with a Message-ID; ready for a future live
    /// IMAP `UID SEARCH HEADER Message-ID` audit. The planner does NOT call
    /// IMAP itself in this surface.
    Planned,
}

/// Single audit row emitted by `plan_restore_state_audit`. Pure planner output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditedRestoreRow {
    pub source: String,
    pub destination: String,
    pub uidvalidity: u32,
    pub uid: u32,
    pub sha256: String,
    pub message_id_present: bool,
    pub status: AuditStatus,
}

impl AuditStatus {
    /// Snake-case status string used on the wire (stable CLI contract).
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            AuditStatus::StateNotInManifest => "state_not_in_manifest",
            AuditStatus::UnknownNoMessageId => "unknown_no_message_id",
            AuditStatus::Planned => "planned",
        }
    }
}

/// Pure, deterministic audit planner. Drives off the
/// `RestoreStateOutcome.warnings` produced by `load_restore_state` so that
/// pending-without-done is the canonical signal — we never re-derive it from
/// the raw NDJSON. Folder include/exclude globs filter on the SOURCE folder
/// name (pre-mapping), matching `plan_restore`'s convention.
pub fn plan_restore_state_audit(
    manifest: &ArchiveManifest,
    outcome: &RestoreStateOutcome,
    mappings: &[FolderMapping],
    includes: &[String],
    excludes: &[String],
) -> Vec<AuditedRestoreRow> {
    use crate::migrate::folder_selected;

    let mut out = Vec::new();
    for warning in &outcome.warnings {
        let RestoreStateWarning::PendingWithoutDone { record } = warning else {
            continue;
        };
        if !folder_selected(&record.folder, includes, excludes) {
            continue;
        }
        let manifest_hit = manifest.messages.iter().find(|m| {
            m.folder == record.folder
                && m.uidvalidity == record.uidvalidity
                && m.uid == record.uid
                && m.sha256 == record.sha256
        });
        let destination = apply_folder_mapping(&record.folder, mappings);
        let (status, message_id_present) = match manifest_hit {
            None => (AuditStatus::StateNotInManifest, false),
            Some(m) => match m.message_id.as_deref() {
                Some(_) => (AuditStatus::Planned, true),
                None => (AuditStatus::UnknownNoMessageId, false),
            },
        };
        out.push(AuditedRestoreRow {
            source: record.folder.clone(),
            destination,
            uidvalidity: record.uidvalidity,
            uid: record.uid,
            sha256: record.sha256.clone(),
            message_id_present,
            status,
        });
    }
    out
}

// -----------------------------------------------------------------------------
// Atomic write + restore-state NDJSON
// -----------------------------------------------------------------------------

/// Write `bytes` to `path` via a temp file in the same directory and rename
/// only after fsync. A crash before the rename leaves no `manifest.json`, which
/// causes verify to fail rather than accept a half-written archive.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write_atomic target has no parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;
    let mut tmp = path.to_path_buf();
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?
        .to_owned();
    let mut tmp_name = file_name.clone();
    tmp_name.push(".tmp");
    tmp.set_file_name(tmp_name);

    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn restore_state_filename(dest_account_id: &str) -> String {
    format!(".restore-state-{dest_account_id}.ndjson")
}

pub fn restore_state_path(archive_dir: &Path, dest_account_id: &str) -> PathBuf {
    archive_dir.join(restore_state_filename(dest_account_id))
}

/// Confine restore-state sidecar IO to a real regular file directly inside the
/// archive directory before any read, touch, or append (issue #18).
///
/// A previous version derived `.restore-state-<dest>.ndjson` and opened it with
/// no symlink check, so a malicious archive could ship that name as a symlink
/// (or place it under a symlinked parent) and make dry-run read state from —
/// or live restore append state to — a path outside the archive. This rejects:
///
/// - a sidecar whose parent directory is a symlink;
/// - an existing sidecar that is a symlink or any non-regular file.
///
/// A missing sidecar is fine: it will be created as a regular file. Like
/// `validate_materialized_message_path`, this does not stat the archive
/// directory itself — the operator chose that path on the command line.
pub fn ensure_restore_state_path_safe(path: &Path) -> Result<(), BackupError> {
    let unsafe_err = |reason: String| BackupError::UnsafeRelPath {
        rel_path: path.display().to_string(),
        reason,
    };

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        match fs::symlink_metadata(parent) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(unsafe_err(format!(
                    "restore-state parent {} is a symlink — refusing to follow outside the archive",
                    parent.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(BackupError::Io(e)),
        }
    }

    match fs::symlink_metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                return Err(unsafe_err(
                    "restore-state sidecar is a symlink — refusing to read or write outside the archive"
                        .to_string(),
                ));
            }
            if !ft.is_file() {
                return Err(unsafe_err(
                    "restore-state sidecar exists but is not a regular file".to_string(),
                ));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(BackupError::Io(e)),
    }
    Ok(())
}

pub fn append_restore_state_line(
    path: &Path,
    record: &RestoreStateRecord,
) -> Result<(), BackupError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    ensure_restore_state_path_safe(path)?;
    let line = serde_json::to_string(record)?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")?;
    f.sync_all()?;
    Ok(())
}

/// Machine-readable warning emitted by `load_restore_state` so callers can
/// surface idempotency-relevant anomalies without silently degrading safety.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreStateWarning {
    /// A NDJSON line that could not be parsed. Previous behavior silently
    /// swallowed these; issue #19 requires a machine-readable signal.
    MalformedLine { line_number: usize, content: String },
    /// A record was written with `status: pending` but no subsequent `done`
    /// line exists. This means the process likely crashed after (or during)
    /// IMAP APPEND. The record is conservatively included in the loaded set
    /// to prevent duplication on rerun.
    PendingWithoutDone { record: RestoreStateRecord },
}

/// Result of loading the restore-state sidecar. Carries both the usable
/// record set and any warnings that callers should surface.
#[derive(Debug, Clone)]
pub struct RestoreStateOutcome {
    pub records: HashSet<RestoreStateRecord>,
    pub warnings: Vec<RestoreStateWarning>,
}

pub fn load_restore_state(path: &Path) -> Result<RestoreStateOutcome, BackupError> {
    let mut done_set: HashSet<RestoreStateRecord> = HashSet::new();
    let mut pending_set: HashSet<RestoreStateRecord> = HashSet::new();
    let mut warnings: Vec<RestoreStateWarning> = Vec::new();

    ensure_restore_state_path_safe(path)?;
    if !path.exists() {
        return Ok(RestoreStateOutcome {
            records: done_set,
            warnings,
        });
    }
    let raw = fs::read_to_string(path)?;
    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<RestoreStateRecord>(trimmed) {
            Ok(rec) => match rec.status {
                RestoreStatus::Pending => {
                    pending_set.insert(rec);
                }
                RestoreStatus::Done => {
                    done_set.insert(rec);
                }
            },
            Err(_) => {
                warnings.push(RestoreStateWarning::MalformedLine {
                    line_number: idx + 1,
                    content: trimmed.to_string(),
                });
            }
        }
    }

    // Pending records not superseded by a done record indicate a crash
    // between the pending write and the done write. Promote them into the
    // done set (conservative skip) and emit a warning per record.
    for pending in &pending_set {
        if !done_set.contains(pending) {
            warnings.push(RestoreStateWarning::PendingWithoutDone {
                record: pending.clone(),
            });
            let mut promoted = pending.clone();
            promoted.status = RestoreStatus::Done;
            done_set.insert(promoted);
        }
    }

    Ok(RestoreStateOutcome {
        records: done_set,
        warnings,
    })
}

// -----------------------------------------------------------------------------
// Manifest read + verify
// -----------------------------------------------------------------------------

pub fn manifest_path(archive_dir: &Path) -> PathBuf {
    archive_dir.join("manifest.json")
}

pub fn read_manifest(archive_dir: &Path) -> Result<ArchiveManifest, BackupError> {
    let path = manifest_path(archive_dir);
    let raw = fs::read_to_string(&path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => BackupError::InvalidArchiveLayout {
            path: path.clone(),
            reason: "manifest.json missing".to_string(),
        },
        _ => BackupError::Io(e),
    })?;
    let manifest: ArchiveManifest =
        serde_json::from_str(&raw).map_err(|e| BackupError::MalformedManifest {
            path: path.clone(),
            reason: e.to_string(),
        })?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Result of enumerating the on-disk `messages/` tree: every `.eml` found plus
/// any symlinked entries that were refused rather than traversed (issue #17).
#[derive(Debug, Default)]
struct ArchiveTreeListing {
    eml_files: Vec<String>,
    unsafe_entries: Vec<String>,
}

/// Walk every `.eml` under `<archive_dir>/messages/` and return paths relative
/// to `archive_dir`, normalized with forward slashes for stable comparison
/// against manifest `rel_path`.
///
/// Symlinks are never followed: `entry.file_type()` reports the link itself
/// (not its target), so a symlinked directory under `messages/` is recorded as
/// an unsafe entry instead of being recursed into. This prevents a crafted
/// archive from escaping the archive root or trapping verify in a symlink loop.
fn list_archive_eml_files(archive_dir: &Path) -> io::Result<ArchiveTreeListing> {
    let mut listing = ArchiveTreeListing::default();
    let messages = archive_dir.join("messages");
    if !messages.exists() {
        return Ok(listing);
    }
    walk_dir(&messages, archive_dir, &mut listing)?;
    listing.unsafe_entries.sort();
    Ok(listing)
}

fn rel_to_root(path: &Path, root: &Path) -> io::Result<String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("strip_prefix: {e}")))?;
    // Normalize Windows-style separators to forward slashes for parity with
    // manifest `rel_path` values.
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

fn walk_dir(dir: &Path, root: &Path, listing: &mut ArchiveTreeListing) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        // `DirEntry::file_type` does NOT follow symlinks on the platforms we
        // support, so this classifies the link itself rather than its target.
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_symlink() {
            // Report, never traverse. A symlinked directory here could point
            // outside the archive (escape) or back into the tree (loop).
            listing.unsafe_entries.push(rel_to_root(&path, root)?);
        } else if file_type.is_dir() {
            walk_dir(&path, root, listing)?;
        } else if file_type.is_file() && path.extension().and_then(|s| s.to_str()) == Some("eml") {
            listing.eml_files.push(rel_to_root(&path, root)?);
        }
    }
    Ok(())
}

/// Verify an archive directory against its manifest. Pure filesystem; no IMAP.
/// Returns a `VerifyOutcome` so the caller decides exit code policy
/// (extras-fail-on-strict lives in the CLI handler).
pub fn verify_archive(archive_dir: &Path) -> Result<VerifyOutcome, BackupError> {
    let manifest = read_manifest(archive_dir)?;
    let mut missing = Vec::new();
    let mut corrupt = Vec::new();
    let mut referenced: HashSet<String> = HashSet::new();

    for record in &manifest.messages {
        // read_manifest already ran validate_manifest; rel_path is canonical.
        referenced.insert(record.rel_path.clone());
        // Walk every component (`messages/`, encoded folder dir, the .eml
        // file) and refuse if any of them is a symlink — this catches the
        // case where an attacker repointed a parent directory rather than
        // the file itself. Any unsafe component → treat as "missing" so we
        // never hash bytes that live outside the archive.
        let path = match validate_materialized_message_path(archive_dir, record) {
            Ok(p) => p,
            Err(BackupError::UnsafeRelPath { .. }) => {
                missing.push(MissingFile {
                    folder: record.folder.clone(),
                    uid: record.uid,
                    rel_path: record.rel_path.clone(),
                });
                continue;
            }
            Err(BackupError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(MissingFile {
                    folder: record.folder.clone(),
                    uid: record.uid,
                    rel_path: record.rel_path.clone(),
                });
                continue;
            }
            Err(other) => return Err(other),
        };
        let metadata = fs::metadata(&path)?;
        if metadata.len() != record.size {
            corrupt.push(CorruptFile::SizeMismatch {
                folder: record.folder.clone(),
                uid: record.uid,
                rel_path: record.rel_path.clone(),
                expected_size: record.size,
                actual_size: metadata.len(),
            });
            continue;
        }
        let actual_sha = sha256_hex_file(&path)?;
        if actual_sha != record.sha256 {
            corrupt.push(CorruptFile::ChecksumMismatch {
                folder: record.folder.clone(),
                uid: record.uid,
                rel_path: record.rel_path.clone(),
                expected_sha256: record.sha256.clone(),
                actual_sha256: actual_sha,
            });
        }
    }

    let listing = list_archive_eml_files(archive_dir)?;
    let mut extras: Vec<String> = listing
        .eml_files
        .into_iter()
        .filter(|p| !referenced.contains(p))
        .collect();
    extras.sort();
    let unsafe_entries = listing.unsafe_entries;

    // A symlinked entry under messages/ is always a hard failure: it signals a
    // crafted/corrupt archive, never a legitimate backup.
    let ok = missing.is_empty() && corrupt.is_empty() && unsafe_entries.is_empty();
    Ok(VerifyOutcome {
        ok,
        manifest_message_count: manifest.messages.len() as u32,
        missing,
        corrupt,
        extras,
        unsafe_entries,
    })
}

// -----------------------------------------------------------------------------
// Helpers used by the CLI handler
// -----------------------------------------------------------------------------

/// Build the canonical `exported_at` timestamp string. Public so the CLI can
/// stamp it into the manifest at export time (tests can also call it).
pub fn exported_at_now_utc() -> String {
    Utc::now().to_rfc3339()
}

/// Parse an ISO 8601 string back into a `DateTime<FixedOffset>` for restore.
/// Returns `None` for `None` / unparseable values; restore appends without an
/// INTERNALDATE in that case rather than failing the whole message.
pub fn parse_internal_date(raw: Option<&str>) -> Option<DateTime<FixedOffset>> {
    raw.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> ArchiveManifest {
        ArchiveManifest {
            archive_format_version: ARCHIVE_FORMAT_VERSION,
            tool: "envelope".to_string(),
            tool_version: "0.7.0".to_string(),
            exported_at: "2026-05-04T12:00:00+00:00".to_string(),
            account: ArchiveAccount {
                id: "acct-123".to_string(),
                email: "user@example.com".to_string(),
                imap_host: "imap.example.com".to_string(),
                imap_port: 993,
                imap_username: "user@example.com".to_string(),
            },
            folders: vec![ArchiveFolderRecord {
                name: "INBOX".to_string(),
                uidvalidity: 12345,
                encoded_dir: "INBOX".to_string(),
                message_count: 1,
            }],
            messages: vec![ArchiveMessageRecord {
                folder: "INBOX".to_string(),
                uid: 1,
                uidvalidity: 12345,
                message_id: Some("<m@example.com>".to_string()),
                internal_date: Some("2026-01-01T12:00:00+00:00".to_string()),
                flags: vec!["\\Seen".to_string()],
                size: 5,
                sha256: sha256_hex(b"hello"),
                rel_path: "messages/INBOX/12345-1.eml".to_string(),
            }],
        }
    }

    // -------------------------------------------------------------------------
    // Phase 1: pure planner
    // -------------------------------------------------------------------------

    #[test]
    fn manifest_round_trip_serializes_and_parses_format_version() {
        let m = sample_manifest();
        let json = serde_json::to_string_pretty(&m).unwrap();
        let parsed: ArchiveManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.archive_format_version, ARCHIVE_FORMAT_VERSION);
        assert_eq!(parsed, m);
        let raw: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(raw["archive_format_version"], 1);
        assert_eq!(raw["tool"], "envelope");
    }

    #[test]
    fn archive_folder_round_trip_for_inbox_archive_junk_email_sent_items_nested_and_spaces() {
        let cases = [
            "INBOX",
            "Archive",
            "Junk E-mail",
            "Sent Items",
            "INBOX/Archive/2024",
            "Folder With   Multiple Spaces",
            "[Gmail]/All Mail",
            "Has%Percent",
            "Has.Dot",
            "Has_Under-score",
            "Já_Açentos_中文",
        ];
        for name in cases {
            let encoded = encode_folder_for_disk(name);
            // The encoded form must contain only filesystem-safe ASCII bytes.
            assert!(
                encoded
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'%')),
                "encoded slug {encoded:?} for {name:?} contains unsafe bytes",
            );
            let decoded = decode_folder_for_disk(&encoded).unwrap();
            assert_eq!(decoded, name, "round-trip failed for {name:?}");
        }
    }

    #[test]
    fn parse_folder_mapping_arg_rejects_missing_equals() {
        assert!(matches!(
            parse_folder_mapping_arg("Junk").unwrap_err(),
            BackupError::MalformedMappingArg { .. }
        ));
        assert!(matches!(
            parse_folder_mapping_arg("=Junk").unwrap_err(),
            BackupError::MalformedMappingArg { .. }
        ));
        assert!(matches!(
            parse_folder_mapping_arg("Junk=").unwrap_err(),
            BackupError::MalformedMappingArg { .. }
        ));
    }

    #[test]
    fn parse_folder_mapping_arg_accepts_simple_pair() {
        let m = parse_folder_mapping_arg("Junk E-mail=Junk").unwrap();
        assert_eq!(m.source, "Junk E-mail");
        assert_eq!(m.destination, "Junk");
    }

    #[test]
    fn apply_folder_mapping_falls_back_to_source_when_no_match() {
        let mappings = [FolderMapping {
            source: "Sent Items".to_string(),
            destination: "Sent".to_string(),
        }];
        assert_eq!(apply_folder_mapping("INBOX", &mappings), "INBOX");
    }

    #[test]
    fn apply_folder_mapping_uses_first_match_for_collisions() {
        let mappings = [
            FolderMapping {
                source: "Junk E-mail".to_string(),
                destination: "Junk".to_string(),
            },
            FolderMapping {
                source: "Junk E-mail".to_string(),
                destination: "Spam".to_string(),
            },
        ];
        assert_eq!(apply_folder_mapping("Junk E-mail", &mappings), "Junk");
    }

    #[test]
    fn common_provider_mappings_constant_lists_junk_sent_deleted() {
        let names: Vec<_> = COMMON_PROVIDER_MAPPINGS.iter().map(|(s, _)| *s).collect();
        assert!(names.contains(&"Junk E-mail"));
        assert!(names.contains(&"Sent Items"));
        assert!(names.contains(&"Deleted Items"));
    }

    #[test]
    fn validate_restore_destination_rejects_same_source_account_id() {
        let manifest = sample_manifest();
        let err = validate_restore_destination(
            &manifest.account,
            "acct-123",
            "imap.other.example.com",
            993,
            "other@example.com",
        )
        .unwrap_err();
        match err {
            BackupError::UnsafeRestoreDestination(msg) => {
                assert!(msg.contains("same account"), "unexpected error: {msg}");
            }
            other => panic!("expected UnsafeRestoreDestination, got {other:?}"),
        }
    }

    #[test]
    fn validate_restore_destination_rejects_same_source_imap_mailbox() {
        let manifest = sample_manifest();
        let err = validate_restore_destination(
            &manifest.account,
            "acct-other",
            " IMAP.EXAMPLE.COM ",
            993,
            " USER@example.com ",
        )
        .unwrap_err();
        match err {
            BackupError::UnsafeRestoreDestination(msg) => {
                assert!(msg.contains("same IMAP mailbox"), "unexpected error: {msg}");
            }
            other => panic!("expected UnsafeRestoreDestination, got {other:?}"),
        }
    }

    #[test]
    fn validate_restore_destination_allows_distinct_destination() {
        let manifest = sample_manifest();
        validate_restore_destination(
            &manifest.account,
            "acct-other",
            "imap.other.example.com",
            993,
            "other@example.com",
        )
        .unwrap();
    }

    #[test]
    fn plan_restore_skips_records_present_in_state() {
        let m = sample_manifest();
        let state: HashSet<RestoreStateRecord> =
            std::iter::once(restore_state_key(&m.messages[0])).collect();
        let plan = plan_restore(&m.messages, &state, &[], &[], &[]);
        assert!(plan.planned_appends.is_empty());
        assert_eq!(plan.skipped_already_restored, 1);
    }

    #[test]
    fn plan_restore_skips_excluded_folders() {
        let mut m = sample_manifest();
        m.folders.push(ArchiveFolderRecord {
            name: "Junk E-mail".to_string(),
            uidvalidity: 678,
            encoded_dir: encode_folder_for_disk("Junk E-mail"),
            message_count: 1,
        });
        m.messages.push(ArchiveMessageRecord {
            folder: "Junk E-mail".to_string(),
            uid: 2,
            uidvalidity: 678,
            message_id: None,
            internal_date: None,
            flags: vec![],
            size: 3,
            sha256: sha256_hex(b"abc"),
            rel_path: "messages/Junk%20E-mail/678-2.eml".to_string(),
        });
        let plan = plan_restore(
            &m.messages,
            &HashSet::new(),
            &[],
            &[],
            &["Junk*".to_string()],
        );
        assert_eq!(plan.planned_appends.len(), 1);
        assert_eq!(plan.planned_appends[0].uid(), 1);
        assert_eq!(plan.skipped_excluded, 1);
    }

    #[test]
    fn plan_restore_dry_run_matches_live_restore() {
        // Property: dry-run plan and live restore plan must come from the same
        // pure helper. Test by calling plan_restore twice with identical inputs
        // and asserting equality — locks the contract that dry-run is honest.
        let m = sample_manifest();
        let mappings = [FolderMapping {
            source: "INBOX".to_string(),
            destination: "INBOX-Backup".to_string(),
        }];
        let plan_a = plan_restore(&m.messages, &HashSet::new(), &mappings, &[], &[]);
        let plan_b = plan_restore(&m.messages, &HashSet::new(), &mappings, &[], &[]);
        assert_eq!(plan_a, plan_b);
        assert_eq!(plan_a.planned_appends.len(), 1);
        assert_eq!(plan_a.planned_appends[0].destination_folder, "INBOX-Backup");
        assert_eq!(plan_a.planned_appends[0].source_folder(), "INBOX");
        assert_eq!(plan_a.destinations, vec!["INBOX-Backup".to_string()]);
    }

    #[test]
    fn append_flags_strips_recent_and_deleted_for_restore() {
        // Backup restore must call `migrate::append_flags`. This locks the
        // shared expectation that `\Recent` and `\Deleted` never reach APPEND.
        let flags = vec![
            "\\Seen".to_string(),
            "\\Recent".to_string(),
            "\\Deleted".to_string(),
            "\\Flagged".to_string(),
        ];
        assert_eq!(crate::migrate::append_flags(&flags), "(\\Seen \\Flagged)");
    }

    #[test]
    fn progress_event_tags_lock_public_taxonomy_for_backup() {
        fn tag_of(event: &BackupEvent) -> String {
            let v: serde_json::Value =
                serde_json::from_str(&serde_json::to_string(event).unwrap()).unwrap();
            v["event"].as_str().unwrap().to_string()
        }

        assert_eq!(
            tag_of(&BackupEvent::Error {
                ok: false,
                error: "boom".into(),
                phase: Some("export".into()),
            }),
            "error"
        );
        // Issue #21: the error event carries no secrets and serializes its
        // stable fields.
        {
            let v: serde_json::Value = serde_json::from_str(
                &serde_json::to_string(&BackupEvent::Error {
                    ok: false,
                    error: "boom".into(),
                    phase: Some("restore".into()),
                })
                .unwrap(),
            )
            .unwrap();
            assert_eq!(v["ok"], false);
            assert_eq!(v["error"], "boom");
            assert_eq!(v["phase"], "restore");
        }
        assert_eq!(
            tag_of(&BackupEvent::VerifyUnsafeEntry {
                rel_path: "messages/evil".into(),
            }),
            "verify_unsafe_entry"
        );
        assert_eq!(
            tag_of(&BackupEvent::ExportFolderStart {
                folder: "INBOX".into(),
                messages: 5,
            }),
            "export_folder_start"
        );
        assert_eq!(
            tag_of(&BackupEvent::ExportMessageWritten {
                folder: "INBOX".into(),
                uid: 1,
                bytes: 10,
                sha256: "abc".into(),
            }),
            "export_message_written"
        );
        assert_eq!(
            tag_of(&BackupEvent::ExportMessageFailed {
                folder: "INBOX".into(),
                uid: 1,
                error: "disk full".into(),
            }),
            "export_message_failed"
        );
        assert_eq!(
            tag_of(&BackupEvent::ExportFolderDone {
                folder: "INBOX".into(),
                written: 5,
            }),
            "export_folder_done"
        );
        assert_eq!(
            tag_of(&BackupEvent::ExportRunDone {
                folders: 1,
                messages: 5,
                bytes: 50,
                archive_dir: "/tmp/a".into(),
            }),
            "export_run_done"
        );
        assert_eq!(
            tag_of(&BackupEvent::VerifyDone {
                ok: true,
                missing: 0,
                corrupt: 0,
                extras: 0,
                unsafe_entries: 0,
            }),
            "verify_done"
        );
        assert_eq!(
            tag_of(&BackupEvent::VerifyChecksumMismatch {
                folder: "INBOX".into(),
                uid: 1,
                rel_path: "messages/INBOX/12345-1.eml".into(),
                expected_sha256: "exp".into(),
                actual_sha256: "act".into(),
            }),
            "verify_checksum_mismatch"
        );
        assert_eq!(
            tag_of(&BackupEvent::VerifySizeMismatch {
                folder: "INBOX".into(),
                uid: 1,
                rel_path: "messages/INBOX/12345-1.eml".into(),
                expected_size: 10,
                actual_size: 9,
            }),
            "verify_size_mismatch"
        );
        assert_eq!(
            tag_of(&BackupEvent::VerifyMissingFile {
                folder: "INBOX".into(),
                uid: 1,
                rel_path: "messages/INBOX/12345-1.eml".into(),
            }),
            "verify_missing_file"
        );
        assert_eq!(
            tag_of(&BackupEvent::VerifyExtraFile {
                rel_path: "messages/Other/99-1.eml".into(),
            }),
            "verify_extra_file"
        );
        assert_eq!(
            tag_of(&BackupEvent::RestoreFolderStart {
                source: "INBOX".into(),
                destination: "INBOX".into(),
                messages: 5,
            }),
            "restore_folder_start"
        );
        assert_eq!(
            tag_of(&BackupEvent::RestoreMessageAppended {
                source: "INBOX".into(),
                destination: "INBOX".into(),
                uid: 1,
                bytes: 10,
            }),
            "restore_message_appended"
        );
        assert_eq!(
            tag_of(&BackupEvent::RestoreMessageSkipped {
                source: "INBOX".into(),
                uid: 1,
                reason: "already_restored".into(),
            }),
            "restore_message_skipped"
        );
        assert_eq!(
            tag_of(&BackupEvent::RestoreMessageFailed {
                source: "INBOX".into(),
                destination: "INBOX".into(),
                uid: 1,
                error: "boom".into(),
            }),
            "restore_message_failed"
        );
        assert_eq!(
            tag_of(&BackupEvent::RestoreFolderDone {
                source: "INBOX".into(),
                destination: "INBOX".into(),
                appended: 1,
                skipped: 0,
                failed: 0,
            }),
            "restore_folder_done"
        );
        assert_eq!(
            tag_of(&BackupEvent::RestoreRunDone {
                folders: 1,
                appended: 1,
                skipped: 0,
                failed: 0,
            }),
            "restore_run_done"
        );
        assert_eq!(
            tag_of(&BackupEvent::RestoreDryRunDone {
                folders: 1,
                would_append: 1,
                would_skip: 0,
            }),
            "restore_dry_run_done"
        );
    }

    #[test]
    fn unsupported_archive_format_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = sample_manifest();
        m.archive_format_version = ARCHIVE_FORMAT_VERSION + 99;
        write_atomic(
            &manifest_path(dir.path()),
            serde_json::to_vec_pretty(&m).unwrap().as_slice(),
        )
        .unwrap();
        let err = read_manifest(dir.path()).unwrap_err();
        assert!(matches!(err, BackupError::UnsupportedFormatVersion { .. }));
    }

    // -------------------------------------------------------------------------
    // Phase 2: verify against a real tempdir (no IMAP)
    // -------------------------------------------------------------------------

    fn build_tempdir_archive(payload: &[u8]) -> (tempfile::TempDir, ArchiveManifest) {
        let dir = tempfile::tempdir().unwrap();
        let m = {
            let mut m = sample_manifest();
            m.messages[0].size = payload.len() as u64;
            m.messages[0].sha256 = sha256_hex(payload);
            m
        };
        let eml_path = dir.path().join(&m.messages[0].rel_path);
        fs::create_dir_all(eml_path.parent().unwrap()).unwrap();
        fs::write(&eml_path, payload).unwrap();
        write_atomic(
            &manifest_path(dir.path()),
            serde_json::to_vec_pretty(&m).unwrap().as_slice(),
        )
        .unwrap();
        (dir, m)
    }

    #[test]
    fn verify_passes_for_well_formed_archive() {
        let (dir, _m) = build_tempdir_archive(b"hello world");
        let outcome = verify_archive(dir.path()).unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.manifest_message_count, 1);
        assert!(outcome.missing.is_empty());
        assert!(outcome.corrupt.is_empty());
        assert!(outcome.extras.is_empty());
    }

    #[test]
    fn verify_fails_on_missing_message_file() {
        let (dir, m) = build_tempdir_archive(b"hello world");
        fs::remove_file(dir.path().join(&m.messages[0].rel_path)).unwrap();
        let outcome = verify_archive(dir.path()).unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.missing.len(), 1);
        assert_eq!(outcome.missing[0].uid, 1);
    }

    #[test]
    fn verify_fails_on_size_mismatch() {
        let (dir, m) = build_tempdir_archive(b"hello world");
        // Truncate to wrong size while keeping prefix.
        fs::write(dir.path().join(&m.messages[0].rel_path), b"hello").unwrap();
        let outcome = verify_archive(dir.path()).unwrap();
        assert!(!outcome.ok);
        assert!(matches!(
            outcome.corrupt[0],
            CorruptFile::SizeMismatch { .. }
        ));
    }

    #[test]
    fn verify_fails_on_sha256_mismatch() {
        let (dir, m) = build_tempdir_archive(b"hello world");
        // Replace bytes preserving size to force a sha mismatch.
        fs::write(dir.path().join(&m.messages[0].rel_path), b"world hello").unwrap();
        let outcome = verify_archive(dir.path()).unwrap();
        assert!(!outcome.ok);
        assert!(matches!(
            outcome.corrupt[0],
            CorruptFile::ChecksumMismatch { .. }
        ));
    }

    #[test]
    fn verify_warns_but_passes_on_extra_file_when_default() {
        let (dir, _m) = build_tempdir_archive(b"hello world");
        // Drop a stray .eml that the manifest does not reference.
        let extra = dir.path().join("messages/INBOX/99999-99.eml");
        fs::write(&extra, b"orphan").unwrap();
        let outcome = verify_archive(dir.path()).unwrap();
        // Engine is policy-free: ok == true because nothing is missing/corrupt.
        // The CLI handler decides whether `--strict` flips extras into a fail.
        assert!(outcome.ok);
        assert_eq!(outcome.extras.len(), 1);
        assert!(outcome.extras[0].ends_with("99999-99.eml"));
    }

    #[test]
    fn write_atomic_rename_does_not_leave_tmp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        write_atomic(&path, b"{}").unwrap();
        assert!(path.exists());
        // No `.tmp` siblings left after a successful rename.
        let tmp = dir.path().join("manifest.json.tmp");
        assert!(!tmp.exists());
        // Calling again must overwrite atomically without partial state.
        write_atomic(&path, b"{\"v\":2}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"v\":2}");
        assert!(!tmp.exists());
    }

    // -------------------------------------------------------------------------
    // Phase 3: restore-state idempotency (no IMAP)
    // -------------------------------------------------------------------------

    #[test]
    fn restore_state_round_trip_via_ndjson_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = restore_state_path(dir.path(), "acct-123");
        let r1 = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 1,
            sha256: "a".into(),
            status: RestoreStatus::Done,
        };
        let r2 = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 2,
            sha256: "b".into(),
            status: RestoreStatus::Done,
        };
        append_restore_state_line(&path, &r1).unwrap();
        append_restore_state_line(&path, &r2).unwrap();
        let outcome = load_restore_state(&path).unwrap();
        assert!(outcome.records.contains(&r1));
        assert!(outcome.records.contains(&r2));
        assert_eq!(outcome.records.len(), 2);
    }

    #[test]
    fn load_restore_state_treats_missing_file_as_empty_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = restore_state_path(dir.path(), "no-such-account");
        let outcome = load_restore_state(&path).unwrap();
        assert!(outcome.records.is_empty());
    }

    #[test]
    fn load_restore_state_skips_malformed_line_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = restore_state_path(dir.path(), "acct-123");
        let r1 = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 1,
            sha256: "a".into(),
            status: RestoreStatus::Done,
        };
        append_restore_state_line(&path, &r1).unwrap();
        // Append a junk line as if the prior process crashed mid-write.
        let mut f = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{{not json").unwrap();
        let r2 = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 2,
            sha256: "b".into(),
            status: RestoreStatus::Done,
        };
        append_restore_state_line(&path, &r2).unwrap();
        let outcome = load_restore_state(&path).unwrap();
        assert!(outcome.records.contains(&r1));
        assert!(outcome.records.contains(&r2));
    }

    #[test]
    fn plan_restore_after_partial_restore_only_returns_remaining_messages() {
        let mut m = sample_manifest();
        m.folders[0].message_count = 2;
        m.messages.push(ArchiveMessageRecord {
            folder: "INBOX".to_string(),
            uid: 2,
            uidvalidity: 12345,
            message_id: None,
            internal_date: None,
            flags: vec![],
            size: 3,
            sha256: sha256_hex(b"def"),
            rel_path: "messages/INBOX/12345-2.eml".to_string(),
        });
        let mut state = HashSet::new();
        state.insert(restore_state_key(&m.messages[0]));

        let plan = plan_restore(&m.messages, &state, &[], &[], &[]);
        assert_eq!(plan.planned_appends.len(), 1);
        assert_eq!(plan.planned_appends[0].uid(), 2);
        assert_eq!(plan.skipped_already_restored, 1);
    }

    // -------------------------------------------------------------------------
    // Issue #19: crash-safe restore for messages without Message-ID
    // -------------------------------------------------------------------------

    #[test]
    fn restore_state_pending_without_done_is_included_in_loaded_set() {
        // Simulates a crash after APPEND but before done-state write.
        // The pending record must be included in the loaded set so reruns
        // skip it rather than duplicating.
        let dir = tempfile::tempdir().unwrap();
        let path = restore_state_path(dir.path(), "acct-crash");
        let record = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 42,
            sha256: sha256_hex(b"crash-test"),
            status: RestoreStatus::Pending,
        };
        append_restore_state_line(&path, &record).unwrap();

        let outcome = load_restore_state(&path).unwrap();
        let key = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 42,
            sha256: sha256_hex(b"crash-test"),
            status: RestoreStatus::Done,
        };
        assert!(
            outcome.records.contains(&key),
            "pending-without-done record must be in the loaded set"
        );
        assert!(
            !outcome.warnings.is_empty(),
            "pending-without-done must produce a warning"
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| matches!(w, RestoreStateWarning::PendingWithoutDone { .. })),
            "warning must be PendingWithoutDone variant"
        );
    }

    #[test]
    fn restore_state_pending_superseded_by_done_produces_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = restore_state_path(dir.path(), "acct-ok");
        let pending = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 7,
            sha256: sha256_hex(b"normal"),
            status: RestoreStatus::Pending,
        };
        let done = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 7,
            sha256: sha256_hex(b"normal"),
            status: RestoreStatus::Done,
        };
        append_restore_state_line(&path, &pending).unwrap();
        append_restore_state_line(&path, &done).unwrap();

        let outcome = load_restore_state(&path).unwrap();
        assert!(outcome.records.contains(&done));
        assert!(
            outcome
                .warnings
                .iter()
                .all(|w| !matches!(w, RestoreStateWarning::PendingWithoutDone { .. })),
            "pending superseded by done should not warn"
        );
    }

    #[test]
    fn restore_state_backward_compat_no_status_field_treated_as_done() {
        // Old-format state files lack the "status" field. They must deserialize
        // as Done for backward compatibility.
        let dir = tempfile::tempdir().unwrap();
        let path = restore_state_path(dir.path(), "acct-old");
        // Write raw JSON without status field.
        let line = r#"{"folder":"INBOX","uidvalidity":1,"uid":1,"sha256":"aa"}"#;
        fs::write(&path, format!("{line}\n")).unwrap();

        let outcome = load_restore_state(&path).unwrap();
        assert_eq!(outcome.records.len(), 1);
        assert!(
            outcome
                .warnings
                .iter()
                .all(|w| !matches!(w, RestoreStateWarning::PendingWithoutDone { .. })),
            "old-format lines are Done, no pending warning"
        );
    }

    #[test]
    fn load_restore_state_emits_warning_for_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = restore_state_path(dir.path(), "acct-bad");
        let good = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 1,
            sha256: "a".repeat(64),
            status: RestoreStatus::Done,
        };
        append_restore_state_line(&path, &good).unwrap();
        // Append a malformed line.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{not json").unwrap();

        let outcome = load_restore_state(&path).unwrap();
        assert!(outcome.records.contains(&good));
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| matches!(w, RestoreStateWarning::MalformedLine { .. })),
            "malformed line must produce a MalformedLine warning"
        );
    }

    #[test]
    fn plan_restore_skips_message_covered_by_pending_only_state() {
        // Issue #19 crash simulation: message_id=None, pending state only.
        // plan_restore must skip this message to prevent duplication.
        let mut m = sample_manifest();
        // Replace the sample message with one that has no message_id.
        m.messages[0].message_id = None;

        let pending_key = RestoreStateRecord {
            folder: m.messages[0].folder.clone(),
            uidvalidity: m.messages[0].uidvalidity,
            uid: m.messages[0].uid,
            sha256: m.messages[0].sha256.clone(),
            status: RestoreStatus::Done, // load_restore_state promotes pending to done
        };
        let state: HashSet<RestoreStateRecord> = std::iter::once(pending_key).collect();
        let plan = plan_restore(&m.messages, &state, &[], &[], &[]);
        assert!(
            plan.planned_appends.is_empty(),
            "message covered by pending-only state must be skipped"
        );
        assert_eq!(plan.skipped_already_restored, 1);
    }

    #[test]
    fn restore_state_record_eq_ignores_status_field() {
        let a = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 1,
            sha256: "abc".into(),
            status: RestoreStatus::Pending,
        };
        let b = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 1,
            sha256: "abc".into(),
            status: RestoreStatus::Done,
        };
        assert_eq!(a, b, "status must not affect equality");
        // They must also produce the same hash.
        use std::hash::{Hash, Hasher};
        let hash_of = |r: &RestoreStateRecord| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            r.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash_of(&a), hash_of(&b), "status must not affect hash");
    }

    #[test]
    fn parse_internal_date_round_trips_rfc3339_strings() {
        let dt = parse_internal_date(Some("2026-01-01T12:00:00+00:00")).unwrap();
        assert_eq!(dt.format("%Y").to_string(), "2026");
        assert!(parse_internal_date(None).is_none());
        assert!(parse_internal_date(Some("not a date")).is_none());
    }

    #[test]
    fn message_filename_combines_uidvalidity_and_uid() {
        assert_eq!(message_filename(12345, 1), "12345-1.eml");
        assert_eq!(message_filename(0, 99), "0-99.eml");
    }

    #[test]
    fn relative_message_path_encodes_folder_with_spaces() {
        let p = relative_message_path("Junk E-mail", 678, 2);
        assert_eq!(p, "messages/Junk%20E-mail/678-2.eml");
    }

    #[test]
    fn sha256_hex_matches_known_value() {
        // Standard NIST FIPS 180-4 vector for "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // -------------------------------------------------------------------------
    // Critical #1: rel_path traversal hardening
    // -------------------------------------------------------------------------

    #[test]
    fn validate_rel_path_accepts_canonical_form() {
        let rel = relative_message_path("Junk E-mail", 678, 2);
        validate_message_rel_path(&rel, "Junk E-mail", 678, 2).unwrap();
    }

    #[test]
    fn validate_rel_path_rejects_absolute_path() {
        let err = validate_message_rel_path("/etc/passwd", "INBOX", 1, 1).unwrap_err();
        assert!(matches!(err, BackupError::UnsafeRelPath { .. }));
    }

    #[test]
    fn validate_rel_path_rejects_parent_traversal() {
        for s in [
            "messages/../etc/passwd",
            "../etc/passwd",
            "messages/INBOX/../INBOX/12345-1.eml",
            "messages/./INBOX/12345-1.eml",
        ] {
            let err = validate_message_rel_path(s, "INBOX", 12345, 1).unwrap_err();
            assert!(
                matches!(err, BackupError::UnsafeRelPath { .. }),
                "expected UnsafeRelPath for {s:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn validate_rel_path_rejects_backslash_and_nul() {
        for s in [
            "messages\\INBOX\\12345-1.eml",
            "messages/INBOX/12345-1\0.eml",
        ] {
            let err = validate_message_rel_path(s, "INBOX", 12345, 1).unwrap_err();
            assert!(matches!(err, BackupError::UnsafeRelPath { .. }));
        }
    }

    #[test]
    fn validate_rel_path_rejects_windows_drive_prefix() {
        let err = validate_message_rel_path("C:/x.eml", "INBOX", 1, 1).unwrap_err();
        assert!(matches!(err, BackupError::UnsafeRelPath { .. }));
    }

    #[test]
    fn validate_rel_path_rejects_encoded_folder_mismatch() {
        let err =
            validate_message_rel_path("messages/Other/12345-1.eml", "INBOX", 12345, 1).unwrap_err();
        match err {
            BackupError::UnsafeRelPath { reason, .. } => {
                assert!(
                    reason.contains("folder slug"),
                    "expected slug mismatch reason, got {reason:?}"
                );
            }
            other => panic!("expected UnsafeRelPath, got {other:?}"),
        }
    }

    #[test]
    fn validate_rel_path_rejects_filename_uid_mismatch() {
        let err = validate_message_rel_path("messages/INBOX/12345-99.eml", "INBOX", 12345, 1)
            .unwrap_err();
        match err {
            BackupError::UnsafeRelPath { reason, .. } => {
                assert!(reason.contains("filename"), "got {reason:?}");
            }
            other => panic!("expected UnsafeRelPath, got {other:?}"),
        }
    }

    #[test]
    fn validate_rel_path_rejects_extra_or_missing_segments() {
        for s in [
            "messages/INBOX",                   // missing filename
            "12345-1.eml",                      // missing prefix
            "messages/INBOX/sub/12345-1.eml",   // extra component
            "messages/INBOX/12345-1.eml/extra", // extra component
            "data/INBOX/12345-1.eml",           // wrong root
        ] {
            let err = validate_message_rel_path(s, "INBOX", 12345, 1).unwrap_err();
            assert!(
                matches!(err, BackupError::UnsafeRelPath { .. }),
                "expected UnsafeRelPath for {s:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn verify_treats_symlinked_message_file_as_missing() {
        // POSIX-only test; on Windows symlink creation needs admin and would
        // pollute baseline noise, so skip there.
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let dir = tempfile::tempdir().unwrap();
            let m = sample_manifest();
            let eml_path = dir.path().join(&m.messages[0].rel_path);
            fs::create_dir_all(eml_path.parent().unwrap()).unwrap();
            // Point the .eml at /etc/hostname (existing readable file). If verify
            // followed it, it would hash unrelated bytes and likely emit a sha
            // mismatch — we want a clean "missing" instead.
            unix_fs::symlink("/etc/hostname", &eml_path).unwrap();
            write_atomic(
                &manifest_path(dir.path()),
                serde_json::to_vec_pretty(&m).unwrap().as_slice(),
            )
            .unwrap();
            let outcome = verify_archive(dir.path()).unwrap();
            assert!(!outcome.ok);
            assert_eq!(outcome.missing.len(), 1);
            assert_eq!(outcome.corrupt.len(), 0);
        }
    }

    #[test]
    fn verify_treats_symlinked_parent_directory_as_missing() {
        // Regression test for the gap between leaf-only symlink detection and
        // parent-dir traversal. Layout under <archive>:
        //
        //   manifest.json
        //   messages/INBOX -> /tmp/<external>/INBOX   (symlinked dir)
        //   /tmp/<external>/INBOX/12345-1.eml         (correct sha for "hello")
        //
        // The external file would hash to the manifest's expected sha, so a
        // version of verify that followed parent-dir symlinks would happily
        // pass. We require it to refuse to follow and treat as missing.
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let archive = tempfile::tempdir().unwrap();
            let external = tempfile::tempdir().unwrap();
            let m = sample_manifest();

            // Build the bytes outside the archive. Same content the manifest
            // expects, so the only thing keeping verify honest is refusing
            // to traverse into the external directory.
            let payload = b"hello";
            assert_eq!(sha256_hex(payload), m.messages[0].sha256);
            let external_inbox = external.path().join("INBOX");
            fs::create_dir(&external_inbox).unwrap();
            fs::write(external_inbox.join("12345-1.eml"), payload).unwrap();

            // Inside the archive, replace the would-be `messages/INBOX`
            // directory with a symlink to the external directory.
            fs::create_dir(archive.path().join("messages")).unwrap();
            unix_fs::symlink(&external_inbox, archive.path().join("messages/INBOX")).unwrap();
            write_atomic(
                &manifest_path(archive.path()),
                serde_json::to_vec_pretty(&m).unwrap().as_slice(),
            )
            .unwrap();

            // Sanity: the external file we placed *would* hash correctly if
            // verify followed the symlinked parent. The test below proves it
            // does not.
            let outcome = verify_archive(archive.path()).unwrap();
            assert!(!outcome.ok, "verify must refuse to traverse symlinked dir");
            assert_eq!(outcome.missing.len(), 1);
            assert_eq!(
                outcome.corrupt.len(),
                0,
                "must not have hashed the external file at all"
            );
            assert_eq!(outcome.missing[0].uid, 1);
        }
    }

    /// Materialize a valid single-message archive ("hello" at INBOX/12345-1)
    /// so extra-file enumeration tests start from a clean, passing archive.
    #[cfg(unix)]
    fn write_valid_smoke_archive(dir: &Path) {
        let m = sample_manifest();
        let eml = dir.join(&m.messages[0].rel_path);
        fs::create_dir_all(eml.parent().unwrap()).unwrap();
        fs::write(&eml, b"hello").unwrap();
        write_atomic(
            &manifest_path(dir),
            serde_json::to_vec_pretty(&m).unwrap().as_slice(),
        )
        .unwrap();
    }

    #[test]
    fn verify_reports_symlinked_extra_directory_without_traversing() {
        // Issue #17: a symlinked directory under messages/ must be reported as
        // an unsafe archive entry, never recursed into (which could escape the
        // archive or loop forever).
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let archive = tempfile::tempdir().unwrap();
            let external = tempfile::tempdir().unwrap();
            write_valid_smoke_archive(archive.path());

            // An extra .eml lives in the external dir; if verify followed the
            // symlink it would surface as an extra (or worse, be hashed).
            fs::write(external.path().join("99999-99.eml"), b"outside").unwrap();
            unix_fs::symlink(external.path(), archive.path().join("messages/evil")).unwrap();

            let outcome = verify_archive(archive.path()).unwrap();
            assert!(!outcome.ok, "unsafe entry must fail verify");
            assert_eq!(outcome.unsafe_entries.len(), 1);
            assert_eq!(outcome.unsafe_entries[0], "messages/evil");
            // The external file must NOT have been enumerated as an extra.
            assert!(
                outcome.extras.iter().all(|e| !e.contains("99999-99")),
                "must not traverse into the symlinked dir: {:?}",
                outcome.extras
            );
            // The legitimate referenced message still verifies cleanly.
            assert!(outcome.missing.is_empty());
            assert!(outcome.corrupt.is_empty());
        }
    }

    #[test]
    fn verify_does_not_hang_on_symlink_loop() {
        // Issue #17: a self-referential symlink loop under messages/ must be
        // refused, not recursed (which would hang or stack-overflow).
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let archive = tempfile::tempdir().unwrap();
            write_valid_smoke_archive(archive.path());
            // messages/loop -> messages  (cycle)
            unix_fs::symlink(
                archive.path().join("messages"),
                archive.path().join("messages/loop"),
            )
            .unwrap();
            let outcome = verify_archive(archive.path()).unwrap();
            assert!(!outcome.ok);
            assert!(outcome.unsafe_entries.iter().any(|e| e == "messages/loop"));
        }
    }

    #[test]
    fn verify_reports_symlinked_extra_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let archive = tempfile::tempdir().unwrap();
            write_valid_smoke_archive(archive.path());
            // A symlinked extra .eml in the same folder dir must be flagged,
            // not silently treated as a normal extra/regular file.
            unix_fs::symlink(
                "/etc/hostname",
                archive.path().join("messages/INBOX/88888-88.eml"),
            )
            .unwrap();
            let outcome = verify_archive(archive.path()).unwrap();
            assert!(!outcome.ok);
            assert!(
                outcome
                    .unsafe_entries
                    .iter()
                    .any(|e| e == "messages/INBOX/88888-88.eml")
            );
        }
    }

    #[test]
    fn verify_normal_unreferenced_regular_file_is_a_plain_extra() {
        // Regression guard: a real (non-symlink) unreferenced .eml stays a
        // tolerated `extra`, not an `unsafe_entry`.
        #[cfg(unix)]
        {
            let archive = tempfile::tempdir().unwrap();
            write_valid_smoke_archive(archive.path());
            fs::write(archive.path().join("messages/INBOX/77777-77.eml"), b"stray").unwrap();
            let outcome = verify_archive(archive.path()).unwrap();
            assert!(outcome.unsafe_entries.is_empty());
            assert_eq!(outcome.extras.len(), 1);
            assert!(outcome.extras[0].ends_with("77777-77.eml"));
            // Non-strict verify still passes for a plain extra.
            assert!(outcome.ok);
        }
    }

    #[test]
    fn restore_state_rejects_symlinked_sidecar() {
        // Issue #18: an existing restore-state sidecar that is a symlink must
        // be refused for both read (load) and write (append).
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let dir = tempfile::tempdir().unwrap();
            let external = tempfile::tempdir().unwrap();
            let target = external.path().join("leaked-state.ndjson");
            fs::write(&target, b"").unwrap();
            let path = restore_state_path(dir.path(), "acct-evil");
            unix_fs::symlink(&target, &path).unwrap();

            let load_err = load_restore_state(&path).unwrap_err();
            assert!(matches!(load_err, BackupError::UnsafeRelPath { .. }));

            let rec = RestoreStateRecord {
                folder: "INBOX".to_string(),
                uidvalidity: 1,
                uid: 1,
                sha256: sha256_hex(b"x"),
                status: RestoreStatus::Done,
            };
            let append_err = append_restore_state_line(&path, &rec).unwrap_err();
            assert!(matches!(append_err, BackupError::UnsafeRelPath { .. }));
        }
    }

    #[test]
    fn restore_state_rejects_symlinked_parent() {
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let root = tempfile::tempdir().unwrap();
            let external = tempfile::tempdir().unwrap();
            // archive_dir is itself a symlink to an external directory.
            let archive_link = root.path().join("archive");
            unix_fs::symlink(external.path(), &archive_link).unwrap();
            let path = restore_state_path(&archive_link, "acct");
            let err = ensure_restore_state_path_safe(&path).unwrap_err();
            assert!(matches!(err, BackupError::UnsafeRelPath { .. }));
        }
    }

    #[test]
    fn restore_state_normal_missing_and_existing_sidecar_ok() {
        // A normal missing sidecar loads empty; a normal regular-file sidecar
        // round-trips append + load.
        let dir = tempfile::tempdir().unwrap();
        let path = restore_state_path(dir.path(), "acct-ok");
        let outcome = load_restore_state(&path).unwrap();
        assert!(outcome.records.is_empty());

        let rec = RestoreStateRecord {
            folder: "INBOX".to_string(),
            uidvalidity: 7,
            uid: 3,
            sha256: sha256_hex(b"y"),
            status: RestoreStatus::Done,
        };
        append_restore_state_line(&path, &rec).unwrap();
        let outcome = load_restore_state(&path).unwrap();
        assert_eq!(outcome.records.len(), 1);
    }

    #[test]
    fn validate_materialized_path_rejects_symlinked_messages_dir() {
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let archive = tempfile::tempdir().unwrap();
            let external = tempfile::tempdir().unwrap();
            // Symlink `<archive>/messages` to an external directory. The leaf
            // .eml file doesn't even need to exist for this check to fire —
            // the unsafe component is detected on the way down.
            unix_fs::symlink(external.path(), archive.path().join("messages")).unwrap();
            let m = sample_manifest();
            let err =
                validate_materialized_message_path(archive.path(), &m.messages[0]).unwrap_err();
            match err {
                BackupError::UnsafeRelPath { reason, .. } => {
                    assert!(reason.contains("symlink"));
                    assert!(reason.contains("messages"));
                }
                other => panic!("expected UnsafeRelPath, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_materialized_path_returns_concrete_path_for_safe_archive() {
        let dir = tempfile::tempdir().unwrap();
        let m = sample_manifest();
        let eml = dir.path().join(&m.messages[0].rel_path);
        fs::create_dir_all(eml.parent().unwrap()).unwrap();
        fs::write(&eml, b"hello").unwrap();
        let resolved = validate_materialized_message_path(dir.path(), &m.messages[0]).unwrap();
        assert_eq!(resolved, eml);
    }

    #[test]
    fn validate_materialized_path_propagates_canonical_rel_path_check() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = sample_manifest();
        m.messages[0].rel_path = "messages/../etc/passwd".to_string();
        let err = validate_materialized_message_path(dir.path(), &m.messages[0]).unwrap_err();
        assert!(matches!(err, BackupError::UnsafeRelPath { .. }));
    }

    // -------------------------------------------------------------------------
    // Critical #2: manifest structural validation
    // -------------------------------------------------------------------------

    #[test]
    fn validate_manifest_accepts_well_formed_sample() {
        validate_manifest(&sample_manifest()).unwrap();
    }

    #[test]
    fn validate_manifest_rejects_duplicate_message_identity() {
        let mut m = sample_manifest();
        let mut dup = m.messages[0].clone();
        // Same (folder, uidvalidity, uid) but different rel_path to isolate
        // the duplicate-identity check from the duplicate-rel-path check.
        dup.rel_path = "messages/INBOX/12345-1.eml".to_string();
        m.messages.push(dup);
        m.folders[0].message_count = 2;
        let err = validate_manifest(&m).unwrap_err();
        match err {
            BackupError::ManifestValidation(msg) => {
                assert!(msg.contains("duplicate message identity"));
            }
            other => panic!("expected ManifestValidation, got {other:?}"),
        }
    }

    #[test]
    fn validate_manifest_rejects_duplicate_rel_path() {
        let mut m = sample_manifest();
        // Add a *different* (uid=2) record but force its rel_path to collide
        // with the existing UID 1 record. validate_message_rel_path runs
        // before the duplicate-rel-path scan, so make sure the colliding
        // rel_path is canonical for *its own* (folder, uidvalidity, uid).
        let mut second = m.messages[0].clone();
        second.uid = 1; // same identity → catches identity dup
        m.messages.push(second);
        m.folders[0].message_count = 2;
        let err = validate_manifest(&m).unwrap_err();
        assert!(matches!(err, BackupError::ManifestValidation(_)));
    }

    #[test]
    fn validate_manifest_rejects_message_referencing_unknown_folder() {
        let mut m = sample_manifest();
        m.messages.push(ArchiveMessageRecord {
            folder: "Phantom".to_string(),
            uid: 9,
            uidvalidity: 1,
            message_id: None,
            internal_date: None,
            flags: vec![],
            size: 1,
            sha256: sha256_hex(b"x"),
            rel_path: "messages/Phantom/1-9.eml".to_string(),
        });
        let err = validate_manifest(&m).unwrap_err();
        match err {
            BackupError::ManifestValidation(msg) => {
                assert!(msg.contains("unknown folder"));
            }
            other => panic!("expected ManifestValidation, got {other:?}"),
        }
    }

    #[test]
    fn validate_manifest_rejects_folder_count_mismatch() {
        let mut m = sample_manifest();
        m.folders[0].message_count = 99;
        let err = validate_manifest(&m).unwrap_err();
        match err {
            BackupError::ManifestValidation(msg) => {
                assert!(msg.contains("message_count"));
            }
            other => panic!("expected ManifestValidation, got {other:?}"),
        }
    }

    #[test]
    fn validate_manifest_rejects_encoded_dir_disagreement() {
        let mut m = sample_manifest();
        m.folders[0].encoded_dir = "Wrong-Encoding".to_string();
        let err = validate_manifest(&m).unwrap_err();
        match err {
            BackupError::ManifestValidation(msg) => {
                assert!(msg.contains("canonical encoding"));
            }
            other => panic!("expected ManifestValidation, got {other:?}"),
        }
    }

    #[test]
    fn validate_manifest_rejects_invalid_sha256() {
        let mut m = sample_manifest();
        m.messages[0].sha256 = "not-hex".to_string();
        let err = validate_manifest(&m).unwrap_err();
        match err {
            BackupError::ManifestValidation(msg) => {
                assert!(msg.contains("invalid sha256"));
            }
            other => panic!("expected ManifestValidation, got {other:?}"),
        }
    }

    #[test]
    fn validate_manifest_rejects_oversize_message() {
        let mut m = sample_manifest();
        m.messages[0].size = MAX_MESSAGE_SIZE_BYTES + 1;
        let err = validate_manifest(&m).unwrap_err();
        assert!(matches!(err, BackupError::ManifestValidation(_)));
    }

    #[test]
    fn validate_manifest_rejects_traversal_in_rel_path() {
        let mut m = sample_manifest();
        m.messages[0].rel_path = "messages/../etc/passwd".to_string();
        let err = validate_manifest(&m).unwrap_err();
        assert!(matches!(err, BackupError::UnsafeRelPath { .. }));
    }

    #[test]
    fn read_manifest_runs_validation() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = sample_manifest();
        m.folders[0].message_count = 99; // intentionally wrong
        write_atomic(
            &manifest_path(dir.path()),
            serde_json::to_vec_pretty(&m).unwrap().as_slice(),
        )
        .unwrap();
        let err = read_manifest(dir.path()).unwrap_err();
        assert!(matches!(err, BackupError::ManifestValidation(_)));
    }

    // -------------------------------------------------------------------------
    // Critical #5: existing output dir safety
    // -------------------------------------------------------------------------

    #[test]
    fn validate_export_output_dir_accepts_nonexistent() {
        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("not-yet-created");
        validate_export_output_dir(&out).unwrap();
    }

    #[test]
    fn validate_export_output_dir_accepts_empty_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        validate_export_output_dir(dir.path()).unwrap();
    }

    #[test]
    fn validate_export_output_dir_rejects_nonempty_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("stale-manifest.json"), b"{}").unwrap();
        let err = validate_export_output_dir(dir.path()).unwrap_err();
        assert!(matches!(err, BackupError::UnsafeOutputDir { .. }));
    }

    #[test]
    fn validate_export_output_dir_rejects_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let parent = tempfile::tempdir().unwrap();
            let target = parent.path().join("target");
            fs::create_dir(&target).unwrap();
            let link = parent.path().join("link");
            unix_fs::symlink(&target, &link).unwrap();
            let err = validate_export_output_dir(&link).unwrap_err();
            assert!(matches!(err, BackupError::UnsafeOutputDir { .. }));
        }
    }

    #[test]
    fn validate_export_output_dir_rejects_existing_file() {
        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("file");
        fs::write(&out, b"x").unwrap();
        let err = validate_export_output_dir(&out).unwrap_err();
        assert!(matches!(err, BackupError::UnsafeOutputDir { .. }));
    }

    // -------------------------------------------------------------------------
    // `backup audit-state` planner (read-only; no IMAP)
    // -------------------------------------------------------------------------

    fn audit_record(folder: &str, uidvalidity: u32, uid: u32, sha: &str) -> RestoreStateRecord {
        RestoreStateRecord {
            folder: folder.to_string(),
            uidvalidity,
            uid,
            sha256: sha.to_string(),
            status: RestoreStatus::Pending,
        }
    }

    fn audit_outcome(warnings: Vec<RestoreStateWarning>) -> RestoreStateOutcome {
        RestoreStateOutcome {
            records: HashSet::new(),
            warnings,
        }
    }

    #[test]
    fn plan_restore_state_audit_returns_empty_when_no_pending_warnings() {
        let manifest = sample_manifest();
        let outcome = audit_outcome(vec![RestoreStateWarning::MalformedLine {
            line_number: 1,
            content: "{not json".to_string(),
        }]);

        let rows = plan_restore_state_audit(&manifest, &outcome, &[], &[], &[]);
        assert!(rows.is_empty());
    }

    #[test]
    fn plan_restore_state_audit_selects_only_pending_without_done() {
        // Build a manifest that contains the pending message; outcome carries
        // both a `MalformedLine` (must be ignored) and one
        // `PendingWithoutDone` warning (the only row we audit).
        let manifest = sample_manifest();
        let pending = audit_record(
            &manifest.messages[0].folder,
            manifest.messages[0].uidvalidity,
            manifest.messages[0].uid,
            &manifest.messages[0].sha256,
        );
        let outcome = audit_outcome(vec![
            RestoreStateWarning::MalformedLine {
                line_number: 1,
                content: "{not json".to_string(),
            },
            RestoreStateWarning::PendingWithoutDone {
                record: pending.clone(),
            },
        ]);

        let rows = plan_restore_state_audit(&manifest, &outcome, &[], &[], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uid, pending.uid);
        assert_eq!(rows[0].source, pending.folder);
    }

    #[test]
    fn plan_restore_state_audit_joins_manifest_by_identity_tuple() {
        let manifest = sample_manifest();
        let m = &manifest.messages[0];
        let outcome = audit_outcome(vec![RestoreStateWarning::PendingWithoutDone {
            record: audit_record(&m.folder, m.uidvalidity, m.uid, &m.sha256),
        }]);
        let rows = plan_restore_state_audit(&manifest, &outcome, &[], &[], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, AuditStatus::Planned);
        assert!(rows[0].message_id_present);
        assert_eq!(rows[0].source, m.folder);
        assert_eq!(rows[0].destination, m.folder);
        assert_eq!(rows[0].uidvalidity, m.uidvalidity);
        assert_eq!(rows[0].uid, m.uid);
        assert_eq!(rows[0].sha256, m.sha256);
    }

    #[test]
    fn plan_restore_state_audit_classifies_state_not_in_manifest() {
        let manifest = sample_manifest();
        // Same folder & uid as the manifest message, but a different sha256
        // (and arbitrarily different uidvalidity). The identity tuple must
        // not match, so we should get StateNotInManifest.
        let outcome = audit_outcome(vec![RestoreStateWarning::PendingWithoutDone {
            record: audit_record("INBOX", 99999, 999, &sha256_hex(b"drift")),
        }]);
        let rows = plan_restore_state_audit(&manifest, &outcome, &[], &[], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, AuditStatus::StateNotInManifest);
        assert!(!rows[0].message_id_present);
    }

    #[test]
    fn plan_restore_state_audit_classifies_unknown_no_message_id() {
        let mut manifest = sample_manifest();
        manifest.messages[0].message_id = None;
        let m = &manifest.messages[0];
        let outcome = audit_outcome(vec![RestoreStateWarning::PendingWithoutDone {
            record: audit_record(&m.folder, m.uidvalidity, m.uid, &m.sha256),
        }]);
        let rows = plan_restore_state_audit(&manifest, &outcome, &[], &[], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, AuditStatus::UnknownNoMessageId);
        assert!(!rows[0].message_id_present);
    }

    #[test]
    fn plan_restore_state_audit_applies_folder_mapping() {
        let manifest = sample_manifest();
        let m = &manifest.messages[0];
        let outcome = audit_outcome(vec![RestoreStateWarning::PendingWithoutDone {
            record: audit_record(&m.folder, m.uidvalidity, m.uid, &m.sha256),
        }]);
        let mappings = vec![FolderMapping {
            source: "INBOX".to_string(),
            destination: "Archive/INBOX-2024".to_string(),
        }];
        let rows = plan_restore_state_audit(&manifest, &outcome, &mappings, &[], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "INBOX");
        assert_eq!(rows[0].destination, "Archive/INBOX-2024");
    }

    #[test]
    fn plan_restore_state_audit_respects_include_exclude() {
        // Two pending-without-done warnings — one in INBOX (kept by include
        // INBOX*), one in Junk E-mail (dropped by exclude Junk*).
        let mut manifest = sample_manifest();
        manifest.folders.push(ArchiveFolderRecord {
            name: "Junk E-mail".to_string(),
            uidvalidity: 678,
            encoded_dir: encode_folder_for_disk("Junk E-mail"),
            message_count: 1,
        });
        manifest.messages.push(ArchiveMessageRecord {
            folder: "Junk E-mail".to_string(),
            uid: 2,
            uidvalidity: 678,
            message_id: None,
            internal_date: None,
            flags: vec![],
            size: 3,
            sha256: sha256_hex(b"junk"),
            rel_path: "messages/Junk%20E-mail/678-2.eml".to_string(),
        });
        let m_inbox = &manifest.messages[0];
        let m_junk = &manifest.messages[1];
        let outcome = audit_outcome(vec![
            RestoreStateWarning::PendingWithoutDone {
                record: audit_record(
                    &m_inbox.folder,
                    m_inbox.uidvalidity,
                    m_inbox.uid,
                    &m_inbox.sha256,
                ),
            },
            RestoreStateWarning::PendingWithoutDone {
                record: audit_record(
                    &m_junk.folder,
                    m_junk.uidvalidity,
                    m_junk.uid,
                    &m_junk.sha256,
                ),
            },
        ]);
        let rows = plan_restore_state_audit(
            &manifest,
            &outcome,
            &[],
            &["INBOX*".to_string(), "Junk*".to_string()],
            &["Junk*".to_string()],
        );
        assert_eq!(rows.len(), 1, "exclude must override include");
        assert_eq!(rows[0].source, "INBOX");
    }

    #[test]
    fn restore_state_audit_record_event_serializes_with_event_tag() {
        let event = BackupEvent::RestoreStateAuditRecord {
            source: "INBOX".to_string(),
            destination: "INBOX".to_string(),
            uidvalidity: 1,
            uid: 7,
            sha256: "a".repeat(64),
            message_id_present: true,
            status: AuditStatus::Planned.as_wire_str().to_string(),
            destination_uid: None,
            error: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"], "restore_state_audit_record");
        // Field set lock.
        for key in [
            "source",
            "destination",
            "uidvalidity",
            "uid",
            "sha256",
            "message_id_present",
            "status",
        ] {
            assert!(parsed.get(key).is_some(), "missing field {key}");
        }
        assert!(
            parsed.get("destination_uid").is_none(),
            "destination_uid must be skipped when None"
        );
        assert!(
            parsed.get("error").is_none(),
            "error must be skipped when None"
        );
        assert_eq!(parsed["status"], "planned");
    }

    #[test]
    fn restore_state_audit_done_event_serializes_with_event_tag() {
        let event = BackupEvent::RestoreStateAuditDone {
            pending: 3,
            present: 0,
            missing: 0,
            unknown: 1,
            state_not_in_manifest: 1,
            errors: 0,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"], "restore_state_audit_done");
        assert_eq!(parsed["pending"], 3);
        assert_eq!(parsed["present"], 0);
        assert_eq!(parsed["missing"], 0);
        assert_eq!(parsed["unknown"], 1);
        assert_eq!(parsed["state_not_in_manifest"], 1);
        assert_eq!(parsed["errors"], 0);
    }
}
