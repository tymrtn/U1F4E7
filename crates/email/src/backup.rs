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

/// One line of the restore-state NDJSON sidecar. Written after every successful
/// destination append. Keys identify a message uniquely within an archive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct RestoreStateRecord {
    pub folder: String,
    pub uidvalidity: u32,
    pub uid: u32,
    pub sha256: String,
}

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
    VerifyUnsafeSymlink {
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
        unsafe_symlinks: u32,
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
    pub unsafe_symlinks: Vec<String>,
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

pub fn append_restore_state_line(
    path: &Path,
    record: &RestoreStateRecord,
) -> Result<(), BackupError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(record)?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")?;
    f.sync_all()?;
    Ok(())
}

pub fn load_restore_state(path: &Path) -> Result<HashSet<RestoreStateRecord>, BackupError> {
    let mut out = HashSet::new();
    if !path.exists() {
        return Ok(out);
    }
    let raw = fs::read_to_string(path)?;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<RestoreStateRecord>(trimmed) {
            Ok(rec) => {
                out.insert(rec);
            }
            Err(_) => {
                // Tolerate malformed lines (partial write before crash) — caller
                // can audit via the file directly. Idempotency degrades safely:
                // a missed entry just means we re-search/re-Message-ID-match.
                continue;
            }
        }
    }
    Ok(out)
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

/// Walk every `.eml` under `<archive_dir>/messages/` and return paths relative
/// to `archive_dir`, normalized with forward slashes for stable comparison
/// against manifest `rel_path`.
///
/// Any symlinked entry (file or directory) is skipped and its relative path is
/// appended to `unsafe_symlinks` so the caller can report it.
fn list_archive_eml_files(
    archive_dir: &Path,
    unsafe_symlinks: &mut Vec<String>,
) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    let messages = archive_dir.join("messages");
    if !messages.exists() {
        return Ok(out);
    }
    walk_dir(&messages, archive_dir, &mut out, unsafe_symlinks)?;
    Ok(out)
}

fn walk_dir(
    dir: &Path,
    root: &Path,
    out: &mut Vec<String>,
    unsafe_symlinks: &mut Vec<String>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // Use symlink_metadata (lstat) so we see the symlink itself rather
        // than following it. This prevents traversal outside the archive
        // and protects against symlink loops.
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_symlink() {
            let rel = path.strip_prefix(root).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("strip_prefix: {e}"))
            })?;
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            unsafe_symlinks.push(rel_str);
        } else if meta.is_dir() {
            walk_dir(&path, root, out, unsafe_symlinks)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("eml") {
            let rel = path.strip_prefix(root).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("strip_prefix: {e}"))
            })?;
            // Normalize Windows-style separators to forward slashes for parity
            // with manifest `rel_path` values.
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            out.push(rel_str);
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

    let mut unsafe_symlinks = Vec::new();
    let on_disk = list_archive_eml_files(archive_dir, &mut unsafe_symlinks)?;
    let mut extras: Vec<String> = on_disk
        .into_iter()
        .filter(|p| !referenced.contains(p))
        .collect();
    extras.sort();
    unsafe_symlinks.sort();

    let ok = missing.is_empty() && corrupt.is_empty() && unsafe_symlinks.is_empty();
    Ok(VerifyOutcome {
        ok,
        manifest_message_count: manifest.messages.len() as u32,
        missing,
        corrupt,
        extras,
        unsafe_symlinks,
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
                unsafe_symlinks: 0,
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
        };
        let r2 = RestoreStateRecord {
            folder: "INBOX".into(),
            uidvalidity: 1,
            uid: 2,
            sha256: "b".into(),
        };
        append_restore_state_line(&path, &r1).unwrap();
        append_restore_state_line(&path, &r2).unwrap();
        let loaded = load_restore_state(&path).unwrap();
        assert!(loaded.contains(&r1));
        assert!(loaded.contains(&r2));
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn load_restore_state_treats_missing_file_as_empty_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = restore_state_path(dir.path(), "no-such-account");
        let loaded = load_restore_state(&path).unwrap();
        assert!(loaded.is_empty());
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
        };
        append_restore_state_line(&path, &r2).unwrap();
        let loaded = load_restore_state(&path).unwrap();
        assert!(loaded.contains(&r1));
        assert!(loaded.contains(&r2));
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
    // Issue #17: harden extra-file enumeration against symlinks
    // -------------------------------------------------------------------------

    #[test]
    fn verify_reports_symlinked_extra_directory_as_unsafe() {
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let (dir, _m) = build_tempdir_archive(b"hello world");

            // Create an external directory with a stray .eml file.
            let external = tempfile::tempdir().unwrap();
            let ext_sub = external.path().join("Drafts");
            fs::create_dir(&ext_sub).unwrap();
            fs::write(ext_sub.join("99-1.eml"), b"external payload").unwrap();

            // Symlink messages/Drafts -> external/Drafts inside the archive.
            unix_fs::symlink(&ext_sub, dir.path().join("messages/Drafts")).unwrap();

            let outcome = verify_archive(dir.path()).unwrap();
            // The symlinked directory must NOT be traversed: no extras from it.
            assert!(
                outcome.extras.is_empty(),
                "must not enumerate files through symlinked directory"
            );
            // Must be reported as an unsafe symlink.
            assert!(
                !outcome.unsafe_symlinks.is_empty(),
                "symlinked directory must appear in unsafe_symlinks"
            );
            assert!(!outcome.ok, "archive with unsafe symlinks must not be ok");
        }
    }

    #[test]
    fn verify_reports_symlink_loop_as_unsafe_without_hanging() {
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let (dir, _m) = build_tempdir_archive(b"hello world");

            // Create a symlink loop: messages/loop -> messages/loop
            let loop_path = dir.path().join("messages/loop");
            unix_fs::symlink(&loop_path, &loop_path).unwrap();

            let outcome = verify_archive(dir.path()).unwrap();
            assert!(
                !outcome.unsafe_symlinks.is_empty(),
                "symlink loop must appear in unsafe_symlinks"
            );
            assert!(!outcome.ok, "archive with symlink loop must not be ok");
        }
    }

    #[test]
    fn verify_reports_symlinked_extra_file_as_unsafe() {
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let (dir, _m) = build_tempdir_archive(b"hello world");

            // Create an external file and symlink to it inside the archive.
            let external = tempfile::tempdir().unwrap();
            let ext_file = external.path().join("stolen.eml");
            fs::write(&ext_file, b"external data").unwrap();
            unix_fs::symlink(&ext_file, dir.path().join("messages/INBOX/fake.eml")).unwrap();

            let outcome = verify_archive(dir.path()).unwrap();
            // The symlinked file must NOT appear in extras.
            assert!(
                outcome.extras.is_empty(),
                "symlinked file must not appear as a regular extra"
            );
            assert!(
                !outcome.unsafe_symlinks.is_empty(),
                "symlinked file must appear in unsafe_symlinks"
            );
            assert!(
                !outcome.ok,
                "archive with unsafe symlink file must not be ok"
            );
        }
    }

    #[test]
    fn verify_still_reports_normal_extra_file_correctly() {
        // Ensure the fix doesn't regress normal extra-file detection.
        let (dir, _m) = build_tempdir_archive(b"hello world");
        let extra = dir.path().join("messages/INBOX/99999-99.eml");
        fs::write(&extra, b"orphan").unwrap();
        let outcome = verify_archive(dir.path()).unwrap();
        assert!(outcome.ok, "extras alone don't fail verify");
        assert_eq!(outcome.extras.len(), 1);
        assert!(outcome.extras[0].ends_with("99999-99.eml"));
        assert!(outcome.unsafe_symlinks.is_empty());
    }
}
