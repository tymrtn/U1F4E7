// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Pure evidence bundle schema, rendering, header-threading, and verification.
//!
//! The CLI layer owns account lookup and IMAP I/O. This module stays testable
//! with synthetic RFC822 bytes and local tempdirs only.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use mail_parser::DateTime as MailDateTime;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::backup;

pub const EVIDENCE_FORMAT_VERSION: u32 = 1;
pub const DEFAULT_MAX_THREAD_MESSAGES: usize = 500;

pub const WARNING_UID_FETCH_MISSING: &str = "uid_fetch_missing";
pub const WARNING_THREAD_EXPANSION_LIMIT: &str = "thread_expansion_limit_reached";

#[derive(Debug, Error)]
pub enum EvidenceError {
    #[error("evidence IO error: {0}")]
    Io(#[from] io::Error),
    #[error("evidence JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("manifest validation failed: {0}")]
    ManifestValidation(String),
    #[error("invalid evidence bundle at {path}: {reason}")]
    InvalidBundle { path: PathBuf, reason: String },
    #[error("invalid evidence query: {0}")]
    QueryValidation(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceManifest {
    pub evidence_format_version: u32,
    pub tool: String,
    pub tool_version: String,
    pub exported_at_utc: String,
    pub account: EvidenceAccount,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub source_store: SourceStoreProvenance,
    pub collection_spec: CollectionSpec,
    pub folders: Vec<EvidenceFolderRecord>,
    pub messages: Vec<EvidenceMessageRecord>,
    pub warnings: Vec<EvidenceWarning>,
    pub stats: EvidenceStats,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceAccount {
    pub id: String,
    pub email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_username: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceStoreProvenance {
    pub credential_backend: String,
    pub app_data_dir: String,
    pub database_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollectionSpec {
    pub folder: String,
    pub compiled_query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_query: Option<String>,
    pub filters: EvidenceQueryFilters,
    pub include_thread: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_thread_messages: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceQueryFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keyword: Vec<String>,
}

impl EvidenceQueryFilters {
    pub fn is_empty(&self) -> bool {
        self.from_address.is_none()
            && self.to_address.is_none()
            && self.subject.is_none()
            && self.since.is_none()
            && self.before.is_none()
            && self.body.is_none()
            && self.keyword.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceFolderRecord {
    pub name: String,
    pub uidvalidity: u32,
    pub encoded_dir: String,
    pub message_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceMessageRecord {
    pub id: String,
    pub thread_id: String,
    pub query_matched: bool,
    pub inclusion_reason: InclusionReason,
    pub folder: String,
    pub uidvalidity: u32,
    pub uid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub internal_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rfc822_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    #[serde(rename = "from", default, skip_serializing_if = "Vec::is_empty")]
    pub from_addr: Vec<String>,
    #[serde(rename = "to", default, skip_serializing_if = "Vec::is_empty")]
    pub to_addr: Vec<String>,
    #[serde(rename = "cc", default, skip_serializing_if = "Vec::is_empty")]
    pub cc_addr: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub flags: Vec<String>,
    pub size: u64,
    pub sha256: String,
    pub rel_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceStats {
    pub matched_messages: u32,
    pub included_messages: u32,
    pub written_messages: u32,
    pub total_bytes: u64,
    pub warnings: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceWarning {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum InclusionReason {
    QueryMatch,
    ThreadAncestor,
    ThreadDescendant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderThreadMessage {
    pub uid: u32,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub subject: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadExpansionMode {
    MatchedOnly,
    FullThread { max_messages: usize },
}

impl ThreadExpansionMode {
    fn is_full_thread(self) -> bool {
        matches!(self, Self::FullThread { .. })
    }

    fn max_messages(self) -> Option<usize> {
        match self {
            Self::MatchedOnly => None,
            Self::FullThread { max_messages } => Some(max_messages),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadExpansionResult {
    pub included: Vec<ExpandedThreadMessage>,
    pub warnings: Vec<EvidenceWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedThreadFetchCandidates {
    pub uids: Vec<u32>,
    pub limit_reached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedThreadMessage {
    pub uid: u32,
    pub thread_id: String,
    pub query_matched: bool,
    pub inclusion_reason: InclusionReason,
}

#[derive(Debug, Clone)]
pub struct EvidenceMessageInput<'a> {
    pub folder: &'a str,
    pub uidvalidity: u32,
    pub uid: u32,
    pub internal_date: Option<String>,
    pub flags: Vec<String>,
    pub rfc822: &'a [u8],
    pub query_matched: bool,
    pub inclusion_reason: InclusionReason,
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceVerifyOutcome {
    pub ok: bool,
    pub manifest_message_count: u32,
    pub missing: Vec<EvidenceMissingFile>,
    pub corrupt: Vec<EvidenceCorruptFile>,
    pub extras: Vec<String>,
    pub top_level_digest_mismatch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceMissingFile {
    pub folder: String,
    pub uid: u32,
    pub rel_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceCorruptFile {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EvidenceEvent {
    CollectFolderStart {
        folder: String,
        query: String,
    },
    CollectMessageWritten {
        folder: String,
        uid: u32,
        bytes: u64,
        sha256: String,
        inclusion_reason: InclusionReason,
    },
    CollectDone {
        folder: String,
        matched: u32,
        included: u32,
        bytes: u64,
        bundle_dir: String,
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
    VerifyExtraFile {
        rel_path: String,
    },
    VerifyBundleDigestMismatch,
    VerifyDone {
        ok: bool,
        missing: u32,
        corrupt: u32,
        extras: u32,
        top_level_digest_mismatch: bool,
    },
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    backup::sha256_hex(bytes)
}

pub fn encode_folder_for_disk(name: &str) -> String {
    backup::encode_folder_for_disk(name)
}

pub fn relative_message_path(folder: &str, uidvalidity: u32, uid: u32) -> String {
    backup::relative_message_path(folder, uidvalidity, uid)
}

pub fn exported_at_now_utc() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn missing_uid_fetch_warnings(
    requested_uids: &[u32],
    fetched_uids: &HashSet<u32>,
    reason: &str,
) -> Vec<EvidenceWarning> {
    let mut missing: Vec<u32> = requested_uids
        .iter()
        .copied()
        .filter(|uid| !fetched_uids.contains(uid))
        .collect();
    missing.sort_unstable();
    missing.dedup();
    missing
        .into_iter()
        .map(|uid| EvidenceWarning {
            code: WARNING_UID_FETCH_MISSING.to_string(),
            message: format!("UID {uid} {reason}"),
            reason: Some(reason.to_string()),
            uid: Some(uid),
            message_id: None,
        })
        .collect()
}

pub fn bounded_thread_fetch_candidates(
    discovered_uids: &[u32],
    loaded_uids: &HashSet<u32>,
    max_thread_messages: usize,
) -> BoundedThreadFetchCandidates {
    let mut candidates: Vec<u32> = discovered_uids
        .iter()
        .copied()
        .filter(|uid| !loaded_uids.contains(uid))
        .collect();
    candidates.sort_unstable();
    candidates.dedup();

    let available = max_thread_messages.saturating_sub(loaded_uids.len());
    let limit_reached = candidates.len() > available;
    candidates.truncate(available);

    BoundedThreadFetchCandidates {
        uids: candidates,
        limit_reached,
    }
}

pub fn thread_expansion_limit_warning(max_thread_messages: usize) -> EvidenceWarning {
    let reason = format!("max_thread_messages limit of {max_thread_messages} reached");
    EvidenceWarning {
        code: WARNING_THREAD_EXPANSION_LIMIT.to_string(),
        message: format!(
            "Header thread expansion reached --max-thread-messages={max_thread_messages}; additional linked messages may be omitted"
        ),
        reason: Some(reason),
        uid: None,
        message_id: None,
    }
}

pub fn compile_search_query(
    raw_query: Option<&str>,
    filters: &EvidenceQueryFilters,
) -> Result<String, EvidenceError> {
    let raw = raw_query.map(str::trim).filter(|s| !s.is_empty());
    if raw_query.is_some() && raw.is_none() {
        return Err(EvidenceError::QueryValidation(
            "--query must not be empty".to_string(),
        ));
    }
    if raw.is_none() && filters.is_empty() {
        return Err(EvidenceError::QueryValidation(
            "provide --query or at least one structured filter".to_string(),
        ));
    }

    let mut terms = Vec::new();
    push_string_term(&mut terms, "FROM", filters.from_address.as_deref())?;
    push_string_term(&mut terms, "TO", filters.to_address.as_deref())?;
    push_string_term(&mut terms, "SUBJECT", filters.subject.as_deref())?;
    push_atom_term(&mut terms, "SINCE", filters.since.as_deref())?;
    push_atom_term(&mut terms, "BEFORE", filters.before.as_deref())?;
    push_string_term(&mut terms, "BODY", filters.body.as_deref())?;
    for keyword in &filters.keyword {
        validate_imap_atom(keyword)?;
        terms.push(format!("KEYWORD {keyword}"));
    }
    if let Some(raw) = raw {
        validate_raw_query(raw)?;
        terms.push(raw.to_string());
    }

    Ok(terms.join(" "))
}

fn push_string_term(
    terms: &mut Vec<String>,
    name: &str,
    value: Option<&str>,
) -> Result<(), EvidenceError> {
    if let Some(value) = value {
        validate_quoted_string(value)?;
        terms.push(format!("{name} {}", imap_quoted_string(value)));
    }
    Ok(())
}

fn push_atom_term(
    terms: &mut Vec<String>,
    name: &str,
    value: Option<&str>,
) -> Result<(), EvidenceError> {
    if let Some(value) = value {
        validate_imap_atom(value)?;
        terms.push(format!("{name} {value}"));
    }
    Ok(())
}

fn validate_raw_query(value: &str) -> Result<(), EvidenceError> {
    if value.contains('\r')
        || value.contains('\n')
        || value.contains('\0')
        || value.contains('{')
        || value.contains('}')
    {
        return Err(EvidenceError::QueryValidation(
            "raw query contains unsupported control or literal characters".to_string(),
        ));
    }
    Ok(())
}

fn validate_quoted_string(value: &str) -> Result<(), EvidenceError> {
    if value.contains('\r') || value.contains('\n') || value.contains('\0') {
        return Err(EvidenceError::QueryValidation(
            "quoted query term contains unsupported control characters".to_string(),
        ));
    }
    Ok(())
}

fn validate_imap_atom(value: &str) -> Result<(), EvidenceError> {
    if value.is_empty()
        || value.bytes().any(|b| {
            b.is_ascii_control()
                || b.is_ascii_whitespace()
                || matches!(b, b'(' | b')' | b'{' | b'}' | b'"' | b'\\')
        })
    {
        return Err(EvidenceError::QueryValidation(format!(
            "invalid IMAP atom {value:?}"
        )));
    }
    Ok(())
}

fn imap_quoted_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', r"\\").replace('"', "\\\""))
}

pub fn validate_manifest(manifest: &EvidenceManifest) -> Result<(), EvidenceError> {
    if manifest.evidence_format_version != EVIDENCE_FORMAT_VERSION {
        return Err(EvidenceError::ManifestValidation(format!(
            "unsupported evidence format version {}",
            manifest.evidence_format_version
        )));
    }
    if manifest.tool.trim().is_empty() {
        return Err(EvidenceError::ManifestValidation(
            "tool must not be empty".to_string(),
        ));
    }
    if manifest.collection_spec.folder.trim().is_empty() {
        return Err(EvidenceError::ManifestValidation(
            "collection_spec.folder must not be empty".to_string(),
        ));
    }
    compile_search_query(
        manifest.collection_spec.raw_query.as_deref(),
        &manifest.collection_spec.filters,
    )?;
    if manifest.collection_spec.compiled_query.trim().is_empty() {
        return Err(EvidenceError::ManifestValidation(
            "collection_spec.compiled_query must not be empty".to_string(),
        ));
    }

    let mut folder_names = HashSet::new();
    for folder in &manifest.folders {
        if !folder_names.insert(folder.name.as_str()) {
            return Err(EvidenceError::ManifestValidation(format!(
                "duplicate folder {:?}",
                folder.name
            )));
        }
        let expected = encode_folder_for_disk(&folder.name);
        if folder.encoded_dir != expected {
            return Err(EvidenceError::ManifestValidation(format!(
                "folder {:?} encoded_dir {:?} should be {:?}",
                folder.name, folder.encoded_dir, expected
            )));
        }
    }

    let mut by_identity = HashSet::new();
    let mut by_rel_path = HashSet::new();
    let mut count_by_folder: HashMap<String, u32> = HashMap::new();
    for message in &manifest.messages {
        if !folder_names.contains(message.folder.as_str()) {
            return Err(EvidenceError::ManifestValidation(format!(
                "message UID {} references unknown folder {:?}",
                message.uid, message.folder
            )));
        }
        if !by_identity.insert((message.folder.clone(), message.uidvalidity, message.uid)) {
            return Err(EvidenceError::ManifestValidation(format!(
                "duplicate message identity {:?}/{}:{}",
                message.folder, message.uidvalidity, message.uid
            )));
        }
        if !by_rel_path.insert(message.rel_path.clone()) {
            return Err(EvidenceError::ManifestValidation(format!(
                "duplicate rel_path {:?}",
                message.rel_path
            )));
        }
        backup::validate_message_rel_path(
            &message.rel_path,
            &message.folder,
            message.uidvalidity,
            message.uid,
        )
        .map_err(|e| EvidenceError::ManifestValidation(e.to_string()))?;
        if !is_valid_sha256_hex(&message.sha256) {
            return Err(EvidenceError::ManifestValidation(format!(
                "message UID {} has invalid sha256",
                message.uid
            )));
        }
        if message.size > backup::MAX_MESSAGE_SIZE_BYTES {
            return Err(EvidenceError::ManifestValidation(format!(
                "message UID {} declares too large size {}",
                message.uid, message.size
            )));
        }
        *count_by_folder.entry(message.folder.clone()).or_insert(0) += 1;
    }
    for folder in &manifest.folders {
        let actual = count_by_folder.get(&folder.name).copied().unwrap_or(0);
        if folder.message_count != actual {
            return Err(EvidenceError::ManifestValidation(format!(
                "folder {:?} declares message_count={} but has {} records",
                folder.name, folder.message_count, actual
            )));
        }
    }
    Ok(())
}

fn is_valid_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn render_index_csv(manifest: &EvidenceManifest) -> Result<String, EvidenceError> {
    validate_manifest(manifest)?;
    let mut out = String::from(
        "id,thread_id,query_matched,inclusion_reason,folder,uidvalidity,uid,internal_date,rfc822_date,message_id,in_reply_to,references,from,to,cc,subject,flags,size,sha256,rel_path\n",
    );
    let mut messages = manifest.messages.clone();
    messages.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    for message in &messages {
        let row = [
            message.id.clone(),
            message.thread_id.clone(),
            message.query_matched.to_string(),
            inclusion_reason_str(&message.inclusion_reason).to_string(),
            message.folder.clone(),
            message.uidvalidity.to_string(),
            message.uid.to_string(),
            message.internal_date.clone().unwrap_or_default(),
            message.rfc822_date.clone().unwrap_or_default(),
            message.message_id.clone().unwrap_or_default(),
            message.in_reply_to.clone().unwrap_or_default(),
            message.references.join(" "),
            message.from_addr.join("; "),
            message.to_addr.join("; "),
            message.cc_addr.join("; "),
            message.subject.clone().unwrap_or_default(),
            message.flags.join(" "),
            message.size.to_string(),
            message.sha256.clone(),
            message.rel_path.clone(),
        ];
        out.push_str(
            &row.iter()
                .map(|field| csv_escape(field))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    Ok(out)
}

fn inclusion_reason_str(reason: &InclusionReason) -> &'static str {
    match reason {
        InclusionReason::QueryMatch => "query_match",
        InclusionReason::ThreadAncestor => "thread_ancestor",
        InclusionReason::ThreadDescendant => "thread_descendant",
    }
}

fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

pub fn render_readme(manifest: &EvidenceManifest) -> String {
    let mut out = String::new();
    out.push_str("# Envelope Evidence Bundle\n\n");
    out.push_str("This bundle preserves raw RFC822 `.eml` messages as canonical originals.\n\n");
    out.push_str("Collection is designed to be read-only: Envelope opens the source folder with IMAP EXAMINE and fetches message bytes with BODY.PEEK[].\n\n");
    out.push_str("Thread expansion is header-driven only using Message-ID, In-Reply-To, and References. Subject text is not used as a fallback.\n\n");
    out.push_str("This bundle intentionally exposes message metadata, account identity, IMAP host/username, and local source paths for provenance. Treat it as sensitive.\n\n");
    out.push_str("SHA-256 hashes in `SHA256SUMS` and `bundle.sha256` provide local tamper evidence, but they are not an external signature.\n\n");
    out.push_str("## Collection\n\n");
    out.push_str(&format!(
        "- Account: {}\n- Folder: {}\n- Query: {}\n- Include thread: {}\n- Messages: {}\n- Bytes: {}\n",
        manifest.account.email,
        manifest.collection_spec.folder,
        manifest.collection_spec.compiled_query,
        manifest.collection_spec.include_thread,
        manifest.stats.written_messages,
        manifest.stats.total_bytes
    ));
    if !manifest.warnings.is_empty() {
        out.push_str("\n## Warnings\n\n");
        for warning in &manifest.warnings {
            out.push_str(&format!("- {}: {}\n", warning.code, warning.message));
        }
    }
    out
}

pub fn render_sha256sums(manifest: &EvidenceManifest) -> String {
    let mut entries: Vec<(String, String)> = manifest
        .messages
        .iter()
        .map(|message| (message.rel_path.clone(), message.sha256.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    render_sha256_entries(&entries)
}

fn render_sha256_entries(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    for (rel_path, sha) in entries {
        out.push_str(sha);
        out.push_str("  ");
        out.push_str(rel_path);
        out.push('\n');
    }
    out
}

pub fn bundle_digest(entries: &[(String, String)]) -> String {
    let mut entries = entries.to_vec();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    sha256_hex(render_sha256_entries(&entries).as_bytes())
}

pub fn write_evidence_bundle(
    root: &Path,
    manifest: &EvidenceManifest,
    messages: &HashMap<String, Vec<u8>>,
) -> Result<(), EvidenceError> {
    validate_manifest(manifest)?;
    backup::validate_export_output_dir(root).map_err(|e| EvidenceError::InvalidBundle {
        path: root.to_path_buf(),
        reason: e.to_string(),
    })?;
    fs::create_dir_all(root)?;

    for record in &manifest.messages {
        let bytes = messages
            .get(&record.rel_path)
            .ok_or_else(|| EvidenceError::InvalidBundle {
                path: root.join(&record.rel_path),
                reason: "missing RFC822 bytes for manifest record".to_string(),
            })?;
        if bytes.len() as u64 != record.size {
            return Err(EvidenceError::InvalidBundle {
                path: root.join(&record.rel_path),
                reason: format!(
                    "message size {} does not match manifest {}",
                    bytes.len(),
                    record.size
                ),
            });
        }
        let actual_sha = sha256_hex(bytes);
        if actual_sha != record.sha256 {
            return Err(EvidenceError::InvalidBundle {
                path: root.join(&record.rel_path),
                reason: format!(
                    "message sha256 {actual_sha} does not match manifest {}",
                    record.sha256
                ),
            });
        }
        backup::write_atomic(&root.join(&record.rel_path), bytes)?;
    }

    let manifest_bytes = serde_json::to_vec_pretty(manifest)?;
    let index = render_index_csv(manifest)?;
    let readme = render_readme(manifest);

    backup::write_atomic(&root.join("manifest.json"), &manifest_bytes)?;
    backup::write_atomic(&root.join("index.csv"), index.as_bytes())?;
    backup::write_atomic(&root.join("README.md"), readme.as_bytes())?;

    let mut entries = vec![
        ("manifest.json".to_string(), sha256_hex(&manifest_bytes)),
        ("index.csv".to_string(), sha256_hex(index.as_bytes())),
        ("README.md".to_string(), sha256_hex(readme.as_bytes())),
    ];
    entries.extend(
        manifest
            .messages
            .iter()
            .map(|record| (record.rel_path.clone(), record.sha256.clone())),
    );
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let sha256sums = render_sha256_entries(&entries);
    backup::write_atomic(&root.join("SHA256SUMS"), sha256sums.as_bytes())?;

    let mut bundle_entries = entries;
    bundle_entries.push(("SHA256SUMS".to_string(), sha256_hex(sha256sums.as_bytes())));
    let digest = bundle_digest(&bundle_entries);
    backup::write_atomic(
        &root.join("bundle.sha256"),
        format!("{digest}\n").as_bytes(),
    )?;

    Ok(())
}

pub fn read_manifest(bundle_dir: &Path) -> Result<EvidenceManifest, EvidenceError> {
    let path = bundle_dir.join("manifest.json");
    let raw = fs::read_to_string(&path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => EvidenceError::InvalidBundle {
            path: path.clone(),
            reason: "manifest.json missing".to_string(),
        },
        _ => EvidenceError::Io(e),
    })?;
    let manifest: EvidenceManifest = serde_json::from_str(&raw)?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

pub fn verify_bundle(
    bundle_dir: &Path,
    strict: bool,
) -> Result<EvidenceVerifyOutcome, EvidenceError> {
    let manifest = read_manifest(bundle_dir)?;
    let mut missing = Vec::new();
    let mut corrupt = Vec::new();
    let mut referenced = HashSet::new();

    for record in &manifest.messages {
        referenced.insert(record.rel_path.clone());
        let path = match validate_materialized_message_path(bundle_dir, record) {
            Ok(path) => path,
            Err(EvidenceError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(EvidenceMissingFile {
                    folder: record.folder.clone(),
                    uid: record.uid,
                    rel_path: record.rel_path.clone(),
                });
                continue;
            }
            Err(EvidenceError::InvalidBundle { .. }) => {
                missing.push(EvidenceMissingFile {
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
            corrupt.push(EvidenceCorruptFile::SizeMismatch {
                folder: record.folder.clone(),
                uid: record.uid,
                rel_path: record.rel_path.clone(),
                expected_size: record.size,
                actual_size: metadata.len(),
            });
            continue;
        }
        let actual_sha = backup::sha256_hex_file(&path)?;
        if actual_sha != record.sha256 {
            corrupt.push(EvidenceCorruptFile::ChecksumMismatch {
                folder: record.folder.clone(),
                uid: record.uid,
                rel_path: record.rel_path.clone(),
                expected_sha256: record.sha256.clone(),
                actual_sha256: actual_sha,
            });
        }
    }

    let mut extras: Vec<String> = list_bundle_eml_files(bundle_dir)?
        .into_iter()
        .filter(|rel_path| !referenced.contains(rel_path))
        .collect();
    extras.sort();

    let top_level_digest_mismatch = match fs::read_to_string(bundle_dir.join("bundle.sha256")) {
        Ok(expected) => {
            let expected = expected.trim();
            match compute_bundle_digest_from_files(bundle_dir, &manifest) {
                Ok(actual) => expected != actual,
                Err(EvidenceError::Io(e)) if e.kind() == io::ErrorKind::NotFound => true,
                Err(EvidenceError::InvalidBundle { .. }) => true,
                Err(other) => return Err(other),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Err(e) => return Err(EvidenceError::Io(e)),
    };

    let ok = missing.is_empty()
        && corrupt.is_empty()
        && !top_level_digest_mismatch
        && (!strict || extras.is_empty());

    Ok(EvidenceVerifyOutcome {
        ok,
        manifest_message_count: manifest.messages.len() as u32,
        missing,
        corrupt,
        extras,
        top_level_digest_mismatch,
    })
}

fn validate_materialized_message_path(
    bundle_dir: &Path,
    record: &EvidenceMessageRecord,
) -> Result<PathBuf, EvidenceError> {
    let backup_record = backup::ArchiveMessageRecord {
        folder: record.folder.clone(),
        uid: record.uid,
        uidvalidity: record.uidvalidity,
        message_id: record.message_id.clone(),
        internal_date: record.internal_date.clone(),
        flags: record.flags.clone(),
        size: record.size,
        sha256: record.sha256.clone(),
        rel_path: record.rel_path.clone(),
    };
    backup::validate_materialized_message_path(bundle_dir, &backup_record).map_err(|e| match e {
        backup::BackupError::Io(e) => EvidenceError::Io(e),
        other => EvidenceError::InvalidBundle {
            path: bundle_dir.join(&record.rel_path),
            reason: other.to_string(),
        },
    })
}

fn list_bundle_eml_files(bundle_dir: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    let messages = bundle_dir.join("messages");
    if messages.exists() {
        walk_dir(&messages, bundle_dir, &mut out)?;
    }
    Ok(out)
}

fn walk_dir(dir: &Path, root: &Path, out: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk_dir(&path, root, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("eml") {
            let rel = path.strip_prefix(root).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("strip_prefix: {e}"))
            })?;
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

fn compute_bundle_digest_from_files(
    bundle_dir: &Path,
    manifest: &EvidenceManifest,
) -> Result<String, EvidenceError> {
    let mut entries = Vec::new();
    for rel in ["manifest.json", "index.csv", "README.md", "SHA256SUMS"] {
        let path = bundle_dir.join(rel);
        entries.push((rel.to_string(), backup::sha256_hex_file(&path)?));
    }
    for record in &manifest.messages {
        let path = validate_materialized_message_path(bundle_dir, record)?;
        entries.push((record.rel_path.clone(), backup::sha256_hex_file(&path)?));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(bundle_digest(&entries))
}

pub fn expand_header_threads(
    messages: &[HeaderThreadMessage],
    matched_uids: &HashSet<u32>,
    mode: ThreadExpansionMode,
) -> ThreadExpansionResult {
    let mut by_uid: HashMap<u32, &HeaderThreadMessage> = HashMap::new();
    let mut by_message_id: HashMap<String, u32> = HashMap::new();
    for message in messages {
        by_uid.insert(message.uid, message);
        if let Some(message_id) = message.message_id.as_deref() {
            by_message_id.insert(message_id.to_string(), message.uid);
        }
    }

    let mut parent_by_uid: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut children_by_uid: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut missing_parent_ids: BTreeMap<String, u32> = BTreeMap::new();
    for message in messages {
        for parent_id in parent_ids(message) {
            if let Some(parent_uid) = by_message_id.get(&parent_id).copied() {
                parent_by_uid
                    .entry(message.uid)
                    .or_default()
                    .push(parent_uid);
                children_by_uid
                    .entry(parent_uid)
                    .or_default()
                    .push(message.uid);
            } else if matched_uids.contains(&message.uid) {
                missing_parent_ids.entry(parent_id).or_insert(message.uid);
            }
        }
    }

    let mut included: HashSet<u32> = matched_uids.clone();
    let mut ancestors = HashSet::new();
    let mut descendants = HashSet::new();

    let max_messages = mode.max_messages();
    let mut limit_reached = max_messages
        .map(|max_messages| matched_uids.len() > max_messages)
        .unwrap_or(false);

    if mode.is_full_thread() {
        let mut queue: VecDeque<u32> = matched_uids.iter().copied().collect();
        while let Some(uid) = queue.pop_front() {
            for parent_uid in parent_by_uid.get(&uid).into_iter().flatten().copied() {
                if ancestors.contains(&parent_uid) {
                    continue;
                }
                if !included.contains(&parent_uid)
                    && max_messages
                        .map(|max_messages| included.len() >= max_messages)
                        .unwrap_or(false)
                {
                    limit_reached = true;
                    continue;
                }
                if ancestors.insert(parent_uid) {
                    included.insert(parent_uid);
                    queue.push_back(parent_uid);
                }
            }
        }

        let mut queue: VecDeque<u32> = matched_uids.iter().copied().collect();
        while let Some(uid) = queue.pop_front() {
            for child_uid in children_by_uid.get(&uid).into_iter().flatten().copied() {
                if descendants.contains(&child_uid) {
                    continue;
                }
                if !included.contains(&child_uid)
                    && max_messages
                        .map(|max_messages| included.len() >= max_messages)
                        .unwrap_or(false)
                {
                    limit_reached = true;
                    continue;
                }
                if descendants.insert(child_uid) {
                    included.insert(child_uid);
                    queue.push_back(child_uid);
                }
            }
        }
    }

    let mut included: Vec<ExpandedThreadMessage> = included
        .into_iter()
        .filter_map(|uid| {
            by_uid.get(&uid).map(|_message| {
                let inclusion_reason = if matched_uids.contains(&uid) {
                    InclusionReason::QueryMatch
                } else if ancestors.contains(&uid) {
                    InclusionReason::ThreadAncestor
                } else {
                    InclusionReason::ThreadDescendant
                };
                ExpandedThreadMessage {
                    uid,
                    thread_id: thread_id_for(uid, &by_uid, &parent_by_uid),
                    query_matched: matched_uids.contains(&uid),
                    inclusion_reason,
                }
            })
        })
        .collect();
    included.sort_by_key(|item| item.uid);

    let mut warnings = Vec::new();
    if mode.is_full_thread() {
        if limit_reached && let Some(max_messages) = max_messages {
            warnings.push(thread_expansion_limit_warning(max_messages));
        }
        warnings.extend(
            missing_parent_ids
                .into_iter()
                .map(|(message_id, uid)| EvidenceWarning {
                    code: "missing_header_parent".to_string(),
                    message: format!("UID {uid} references missing parent {message_id}"),
                    reason: Some(
                        "referenced parent message was not fetched or not present in the selected folder"
                            .to_string(),
                    ),
                    uid: Some(uid),
                    message_id: Some(message_id),
                }),
        );
    }

    ThreadExpansionResult { included, warnings }
}

fn parent_ids(message: &HeaderThreadMessage) -> Vec<String> {
    let mut out = Vec::new();
    for value in &message.references {
        out.extend(extract_message_ids(value));
    }
    if let Some(in_reply_to) = message.in_reply_to.as_deref() {
        out.extend(extract_message_ids(in_reply_to));
    }
    let mut seen = HashSet::new();
    out.retain(|id| seen.insert(id.clone()));
    out
}

pub fn extract_message_ids(value: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut start = None;
    for (idx, ch) in value.char_indices() {
        if ch == '<' {
            start = Some(idx);
        } else if ch == '>'
            && let Some(s) = start.take()
        {
            ids.push(value[s..=idx].to_string());
        }
    }
    if ids.is_empty() && !value.trim().is_empty() {
        ids.push(value.trim().to_string());
    }
    ids
}

fn thread_id_for(
    uid: u32,
    by_uid: &HashMap<u32, &HeaderThreadMessage>,
    parent_by_uid: &HashMap<u32, Vec<u32>>,
) -> String {
    let mut current = uid;
    let mut visited = HashSet::new();
    while visited.insert(current) {
        if let Some(parent_uid) = parent_by_uid
            .get(&current)
            .and_then(|parents| parents.first())
            .copied()
        {
            current = parent_uid;
        } else {
            break;
        }
    }
    by_uid
        .get(&current)
        .and_then(|message| message.message_id.clone())
        .or_else(|| {
            by_uid
                .get(&uid)
                .and_then(|message| message.message_id.clone())
        })
        .unwrap_or_else(|| format!("uid:{uid}"))
}

pub fn message_record_from_rfc822(input: EvidenceMessageInput<'_>) -> EvidenceMessageRecord {
    let parsed = mail_parser::MessageParser::default().parse(input.rfc822);
    let message_id = parsed
        .as_ref()
        .and_then(|message| message.message_id().map(|s| s.to_string()));
    let in_reply_to = parsed.as_ref().and_then(|message| {
        message
            .in_reply_to()
            .as_text()
            .map(|value| value.to_string())
    });
    let references = parsed
        .as_ref()
        .and_then(|message| message.references().as_text().map(extract_message_ids))
        .unwrap_or_default();
    let rfc822_date = parsed
        .as_ref()
        .and_then(|message| message.date().map(mail_date_to_rfc3339));
    let from_addr = parsed
        .as_ref()
        .map(|message| address_list(message.from()))
        .unwrap_or_default();
    let to_addr = parsed
        .as_ref()
        .map(|message| address_list(message.to()))
        .unwrap_or_default();
    let cc_addr = parsed
        .as_ref()
        .map(|message| address_list(message.cc()))
        .unwrap_or_default();
    let subject = parsed
        .as_ref()
        .and_then(|message| message.subject().map(|s| s.to_string()));

    EvidenceMessageRecord {
        id: format!("{}:{}:{}", input.folder, input.uidvalidity, input.uid),
        thread_id: input.thread_id,
        query_matched: input.query_matched,
        inclusion_reason: input.inclusion_reason,
        folder: input.folder.to_string(),
        uidvalidity: input.uidvalidity,
        uid: input.uid,
        internal_date: input.internal_date,
        rfc822_date,
        message_id,
        in_reply_to,
        references,
        from_addr,
        to_addr,
        cc_addr,
        subject,
        flags: input.flags,
        size: input.rfc822.len() as u64,
        sha256: sha256_hex(input.rfc822),
        rel_path: relative_message_path(input.folder, input.uidvalidity, input.uid),
    }
}

pub fn header_thread_message_from_rfc822(uid: u32, rfc822: &[u8]) -> HeaderThreadMessage {
    let parsed = mail_parser::MessageParser::default().parse(rfc822);
    HeaderThreadMessage {
        uid,
        message_id: parsed
            .as_ref()
            .and_then(|message| message.message_id().map(|s| s.to_string())),
        in_reply_to: parsed.as_ref().and_then(|message| {
            message
                .in_reply_to()
                .as_text()
                .map(|value| value.to_string())
        }),
        references: parsed
            .as_ref()
            .and_then(|message| message.references().as_text().map(extract_message_ids))
            .unwrap_or_default(),
        subject: parsed
            .as_ref()
            .and_then(|message| message.subject().map(|s| s.to_string())),
    }
}

fn mail_date_to_rfc3339(date: &MailDateTime) -> String {
    date.to_rfc3339()
}

fn address_list(header: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    match header {
        Some(mail_parser::Address::List(list)) => list
            .iter()
            .filter_map(|address| address.address.as_ref().map(|addr| addr.to_string()))
            .collect(),
        Some(mail_parser::Address::Group(groups)) => groups
            .iter()
            .flat_map(|group| group.addresses.iter())
            .filter_map(|address| address.address.as_ref().map(|addr| addr.to_string()))
            .collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::Path;

    fn sample_rfc822() -> Vec<u8> {
        b"Message-ID: <match@example.com>\r\nIn-Reply-To: <root@example.com>\r\nReferences: <root@example.com>\r\nDate: Mon, 05 Jan 2026 10:00:00 +0000\r\nFrom: Sender <sender@example.com>\r\nTo: Recipient <recipient@example.com>\r\nCc: Legal <legal@example.com>\r\nSubject: Contract, signed\r\n\r\nBody\r\n".to_vec()
    }

    fn sample_manifest_for(bytes: &[u8]) -> EvidenceManifest {
        EvidenceManifest {
            evidence_format_version: EVIDENCE_FORMAT_VERSION,
            tool: "envelope".to_string(),
            tool_version: "0.7.0".to_string(),
            exported_at_utc: "2026-05-05T12:00:00Z".to_string(),
            account: EvidenceAccount {
                id: "acct-1".to_string(),
                email: "user@example.com".to_string(),
                imap_host: Some("imap.example.com".to_string()),
                imap_port: Some(993),
                imap_username: Some("user@example.com".to_string()),
            },
            provider: Some("generic".to_string()),
            source_store: SourceStoreProvenance {
                credential_backend: "file".to_string(),
                app_data_dir: "/Users/test/.config/envelope-email".to_string(),
                database_path: "/Users/test/.config/envelope-email/envelope.db".to_string(),
                home: Some("/Users/test".to_string()),
                warnings: vec![],
            },
            collection_spec: CollectionSpec {
                folder: "[Gmail]/All Mail".to_string(),
                compiled_query: r#"FROM "sender@example.com" SUBJECT "contract""#.to_string(),
                raw_query: Some(r#"SUBJECT "contract""#.to_string()),
                filters: EvidenceQueryFilters {
                    from_address: Some("sender@example.com".to_string()),
                    ..EvidenceQueryFilters::default()
                },
                include_thread: true,
                max_thread_messages: Some(DEFAULT_MAX_THREAD_MESSAGES),
            },
            folders: vec![EvidenceFolderRecord {
                name: "[Gmail]/All Mail".to_string(),
                uidvalidity: 777,
                encoded_dir: "%5BGmail%5D%2FAll%20Mail".to_string(),
                message_count: 1,
            }],
            messages: vec![EvidenceMessageRecord {
                id: "[Gmail]/All Mail:777:42".to_string(),
                thread_id: "<root@example.com>".to_string(),
                query_matched: true,
                inclusion_reason: InclusionReason::QueryMatch,
                folder: "[Gmail]/All Mail".to_string(),
                uidvalidity: 777,
                uid: 42,
                internal_date: Some("2026-01-05T10:01:00+00:00".to_string()),
                rfc822_date: Some("Mon, 05 Jan 2026 10:00:00 +0000".to_string()),
                message_id: Some("<match@example.com>".to_string()),
                in_reply_to: Some("<root@example.com>".to_string()),
                references: vec!["<root@example.com>".to_string()],
                from_addr: vec!["sender@example.com".to_string()],
                to_addr: vec!["recipient@example.com".to_string()],
                cc_addr: vec!["legal@example.com".to_string()],
                subject: Some("Contract, signed".to_string()),
                flags: vec!["\\Seen".to_string()],
                size: bytes.len() as u64,
                sha256: sha256_hex(bytes),
                rel_path: "messages/%5BGmail%5D%2FAll%20Mail/777-42.eml".to_string(),
            }],
            warnings: vec![],
            stats: EvidenceStats {
                matched_messages: 1,
                included_messages: 1,
                written_messages: 1,
                total_bytes: bytes.len() as u64,
                warnings: 0,
            },
        }
    }

    fn write_sample_bundle(root: &Path) -> EvidenceManifest {
        let bytes = sample_rfc822();
        let manifest = sample_manifest_for(&bytes);
        let mut messages = HashMap::new();
        messages.insert(manifest.messages[0].rel_path.clone(), bytes);
        write_evidence_bundle(root, &manifest, &messages).unwrap();
        manifest
    }

    #[test]
    fn manifest_round_trip_schema_validates_expected_top_level_fields() {
        let bytes = sample_rfc822();
        let manifest = sample_manifest_for(&bytes);

        validate_manifest(&manifest).unwrap();
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let parsed: EvidenceManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, manifest);

        let raw: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(raw["evidence_format_version"], EVIDENCE_FORMAT_VERSION);
        assert_eq!(raw["tool"], "envelope");
        assert_eq!(raw["collection_spec"]["folder"], "[Gmail]/All Mail");
        assert_eq!(
            raw["messages"][0]["rel_path"],
            "messages/%5BGmail%5D%2FAll%20Mail/777-42.eml"
        );
    }

    #[test]
    fn index_csv_renders_stable_header_and_escapes_fields() {
        let manifest = sample_manifest_for(&sample_rfc822());
        let csv = render_index_csv(&manifest).unwrap();

        assert!(csv.starts_with("id,thread_id,query_matched,inclusion_reason,folder,uidvalidity,uid,internal_date,rfc822_date,message_id,in_reply_to,references,from,to,cc,subject,flags,size,sha256,rel_path\n"));
        assert!(csv.contains("query_match"));
        assert!(csv.contains("\"Contract, signed\""));
        assert!(csv.contains("messages/%5BGmail%5D%2FAll%20Mail/777-42.eml"));
    }

    #[test]
    fn readme_sha256sums_bundle_digest_and_required_files_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_sample_bundle(dir.path());

        for rel in [
            "manifest.json",
            "index.csv",
            "README.md",
            "SHA256SUMS",
            "bundle.sha256",
            manifest.messages[0].rel_path.as_str(),
        ] {
            assert!(dir.path().join(rel).exists(), "missing {rel}");
        }

        let readme = fs::read_to_string(dir.path().join("README.md")).unwrap();
        assert!(readme.contains("raw RFC822"));
        assert!(readme.contains("read-only"));
        assert!(readme.contains("intentionally exposes message metadata"));
        assert!(readme.contains("not an external signature"));

        let sums = fs::read_to_string(dir.path().join("SHA256SUMS")).unwrap();
        assert!(sums.contains(&manifest.messages[0].sha256));
        assert!(sums.contains(&manifest.messages[0].rel_path));

        let digest = fs::read_to_string(dir.path().join("bundle.sha256")).unwrap();
        assert_eq!(digest.trim().len(), 64);

        let outcome = verify_bundle(dir.path(), false).unwrap();
        assert!(
            outcome.ok,
            "fresh synthetic bundle should verify: {outcome:?}"
        );
    }

    #[test]
    fn bundle_digest_is_deterministic_over_sorted_path_hash_entries() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let forward = bundle_digest(&[
            ("b.txt".to_string(), b.clone()),
            ("a.txt".to_string(), a.clone()),
        ]);
        let reverse = bundle_digest(&[("a.txt".to_string(), a), ("b.txt".to_string(), b)]);

        assert_eq!(forward, reverse);
        assert_ne!(
            forward,
            bundle_digest(&[("a.txt".to_string(), "c".repeat(64))])
        );
    }

    #[test]
    fn manifest_and_generated_text_do_not_serialize_secret_fields() {
        let manifest = sample_manifest_for(&sample_rfc822());
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let readme = render_readme(&manifest);
        let csv = render_index_csv(&manifest).unwrap();

        for rendered in [&json, &readme, &csv] {
            for forbidden in [
                "imap_password",
                "smtp_password",
                "oauth_access_token",
                "oauth_refresh_token",
                "authorization",
                "credential_file_path",
                "webhook",
            ] {
                assert!(
                    !rendered.to_lowercase().contains(forbidden),
                    "forbidden field marker {forbidden} appeared in rendered output"
                );
            }
        }
    }

    #[test]
    fn source_store_provenance_serializes_paths_without_secret_material() {
        let provenance = SourceStoreProvenance {
            credential_backend: "keychain".to_string(),
            app_data_dir: "/Users/test/.config/envelope-email".to_string(),
            database_path: "/Users/test/.config/envelope-email/envelope.db".to_string(),
            home: Some("/Users/test".to_string()),
            warnings: vec!["HOME changed between runs".to_string()],
        };

        let value = serde_json::to_value(&provenance).unwrap();
        assert!(value.get("credential_backend").is_some());
        assert!(value.get("app_data_dir").is_some());
        assert!(value.get("database_path").is_some());
        assert!(value.get("home").is_some());
        assert!(value.get("password").is_none());
        assert!(value.get("token").is_none());
        assert!(value.get("secret").is_none());
        assert!(value.get("credential_file_path").is_none());

        let rendered = serde_json::to_string_pretty(&provenance).unwrap();
        for forbidden in ["password", "access_token", "refresh_token", "client_secret"] {
            assert!(
                !rendered.to_lowercase().contains(forbidden),
                "forbidden secret marker {forbidden} appeared in source_store provenance"
            );
        }
    }

    #[test]
    fn query_filters_compile_to_stable_imap_search_terms() {
        let filters = EvidenceQueryFilters {
            from_address: Some("sender@example.com".to_string()),
            to_address: Some("recipient@example.com".to_string()),
            subject: Some("contract".to_string()),
            since: Some("1-Jan-2026".to_string()),
            before: Some("1-Feb-2026".to_string()),
            body: Some("payment terms".to_string()),
            keyword: vec!["Flagged".to_string()],
        };

        assert_eq!(
            compile_search_query(None, &filters).unwrap(),
            r#"FROM "sender@example.com" TO "recipient@example.com" SUBJECT "contract" SINCE 1-Jan-2026 BEFORE 1-Feb-2026 BODY "payment terms" KEYWORD Flagged"#
        );
    }

    #[test]
    fn raw_query_combines_with_structured_terms_as_outer_implicit_and() {
        let filters = EvidenceQueryFilters {
            from_address: Some("sender@example.com".to_string()),
            ..EvidenceQueryFilters::default()
        };

        assert_eq!(
            compile_search_query(Some(r#"SUBJECT "contract""#), &filters).unwrap(),
            r#"FROM "sender@example.com" SUBJECT "contract""#
        );
    }

    #[test]
    fn query_validation_rejects_empty_or_missing_filters_but_all_is_explicit() {
        assert!(compile_search_query(None, &EvidenceQueryFilters::default()).is_err());
        assert!(compile_search_query(Some(""), &EvidenceQueryFilters::default()).is_err());
        assert_eq!(
            compile_search_query(Some("ALL"), &EvidenceQueryFilters::default()).unwrap(),
            "ALL"
        );
    }

    #[test]
    fn query_compilation_escapes_quoted_string_terms() {
        let filters = EvidenceQueryFilters {
            subject: Some(r#"contract "final" \ review"#.to_string()),
            ..EvidenceQueryFilters::default()
        };

        assert_eq!(
            compile_search_query(None, &filters).unwrap(),
            r#"SUBJECT "contract \"final\" \\ review""#
        );
    }

    #[test]
    fn missing_uid_fetch_warnings_include_uid_and_reason() {
        let warnings = missing_uid_fetch_warnings(
            &[3, 1, 3, 2],
            &HashSet::from([2]),
            "returned by UID SEARCH but absent from UID FETCH results",
        );

        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].code, WARNING_UID_FETCH_MISSING);
        assert_eq!(warnings[0].uid, Some(1));
        assert_eq!(
            warnings[0].reason.as_deref(),
            Some("returned by UID SEARCH but absent from UID FETCH results")
        );
        assert_eq!(warnings[1].uid, Some(3));
    }

    #[test]
    fn bounded_thread_fetch_candidates_stop_at_max_thread_messages() {
        let plan = bounded_thread_fetch_candidates(&[5, 4, 3, 4], &HashSet::from([1, 2]), 4);

        assert_eq!(plan.uids, vec![3, 4]);
        assert!(plan.limit_reached);

        let closed = bounded_thread_fetch_candidates(&[6], &HashSet::from([1, 2]), 2);
        assert!(closed.uids.is_empty());
        assert!(closed.limit_reached);
    }

    #[test]
    fn header_thread_expansion_keeps_matched_only_without_include_thread() {
        let messages = vec![
            HeaderThreadMessage {
                uid: 1,
                message_id: Some("<root@example.com>".to_string()),
                in_reply_to: None,
                references: vec![],
                subject: Some("Same subject".to_string()),
            },
            HeaderThreadMessage {
                uid: 2,
                message_id: Some("<match@example.com>".to_string()),
                in_reply_to: Some("<root@example.com>".to_string()),
                references: vec!["<root@example.com>".to_string()],
                subject: Some("Same subject".to_string()),
            },
        ];
        let result = expand_header_threads(
            &messages,
            &HashSet::from([2]),
            ThreadExpansionMode::MatchedOnly,
        );

        assert_eq!(result.included.len(), 1);
        assert_eq!(result.included[0].uid, 2);
        assert_eq!(
            result.included[0].inclusion_reason,
            InclusionReason::QueryMatch
        );
    }

    #[test]
    fn header_thread_expansion_tags_ancestors_and_descendants() {
        let messages = vec![
            HeaderThreadMessage {
                uid: 1,
                message_id: Some("<root@example.com>".to_string()),
                in_reply_to: None,
                references: vec![],
                subject: Some("Unrelated subject text".to_string()),
            },
            HeaderThreadMessage {
                uid: 2,
                message_id: Some("<match@example.com>".to_string()),
                in_reply_to: Some("<root@example.com>".to_string()),
                references: vec!["<root@example.com>".to_string()],
                subject: Some("Contract".to_string()),
            },
            HeaderThreadMessage {
                uid: 3,
                message_id: Some("<child@example.com>".to_string()),
                in_reply_to: Some("<match@example.com>".to_string()),
                references: vec![
                    "<root@example.com>".to_string(),
                    "<match@example.com>".to_string(),
                ],
                subject: Some("Different subject".to_string()),
            },
        ];

        let result = expand_header_threads(
            &messages,
            &HashSet::from([2]),
            ThreadExpansionMode::FullThread {
                max_messages: DEFAULT_MAX_THREAD_MESSAGES,
            },
        );
        let reasons: HashMap<u32, InclusionReason> = result
            .included
            .iter()
            .map(|item| (item.uid, item.inclusion_reason.clone()))
            .collect();

        assert_eq!(reasons.get(&1), Some(&InclusionReason::ThreadAncestor));
        assert_eq!(reasons.get(&2), Some(&InclusionReason::QueryMatch));
        assert_eq!(reasons.get(&3), Some(&InclusionReason::ThreadDescendant));
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn header_thread_expansion_respects_max_thread_messages_and_warns() {
        let messages = vec![
            HeaderThreadMessage {
                uid: 1,
                message_id: Some("<root@example.com>".to_string()),
                in_reply_to: None,
                references: vec![],
                subject: Some("Root".to_string()),
            },
            HeaderThreadMessage {
                uid: 2,
                message_id: Some("<match@example.com>".to_string()),
                in_reply_to: Some("<root@example.com>".to_string()),
                references: vec!["<root@example.com>".to_string()],
                subject: Some("Match".to_string()),
            },
            HeaderThreadMessage {
                uid: 3,
                message_id: Some("<child@example.com>".to_string()),
                in_reply_to: Some("<match@example.com>".to_string()),
                references: vec![
                    "<root@example.com>".to_string(),
                    "<match@example.com>".to_string(),
                ],
                subject: Some("Child".to_string()),
            },
        ];

        let result = expand_header_threads(
            &messages,
            &HashSet::from([2]),
            ThreadExpansionMode::FullThread { max_messages: 2 },
        );
        let included: HashSet<u32> = result.included.iter().map(|item| item.uid).collect();

        assert_eq!(included, HashSet::from([1, 2]));
        assert!(result.warnings.iter().any(|warning| {
            warning.code == WARNING_THREAD_EXPANSION_LIMIT
                && warning
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("max_thread_messages"))
        }));
    }

    #[test]
    fn header_thread_expansion_warns_for_missing_parent_links() {
        let messages = vec![HeaderThreadMessage {
            uid: 2,
            message_id: Some("<match@example.com>".to_string()),
            in_reply_to: Some("<missing@example.com>".to_string()),
            references: vec!["<missing@example.com>".to_string()],
            subject: Some("Contract".to_string()),
        }];

        let result = expand_header_threads(
            &messages,
            &HashSet::from([2]),
            ThreadExpansionMode::FullThread {
                max_messages: DEFAULT_MAX_THREAD_MESSAGES,
            },
        );

        assert_eq!(result.included.len(), 1);
        assert!(result.warnings.iter().any(|warning| {
            warning.code == "missing_header_parent"
                && warning.message.contains("<missing@example.com>")
        }));
    }

    #[test]
    fn header_thread_expansion_never_falls_back_to_subject_matching() {
        let messages = vec![
            HeaderThreadMessage {
                uid: 1,
                message_id: Some("<match@example.com>".to_string()),
                in_reply_to: None,
                references: vec![],
                subject: Some("Contract thread".to_string()),
            },
            HeaderThreadMessage {
                uid: 2,
                message_id: Some("<subject-only@example.com>".to_string()),
                in_reply_to: None,
                references: vec![],
                subject: Some("Contract thread".to_string()),
            },
        ];

        let result = expand_header_threads(
            &messages,
            &HashSet::from([1]),
            ThreadExpansionMode::FullThread {
                max_messages: DEFAULT_MAX_THREAD_MESSAGES,
            },
        );
        let included: HashSet<u32> = result.included.iter().map(|item| item.uid).collect();

        assert_eq!(included, HashSet::from([1]));
    }

    #[test]
    fn verify_reports_missing_eml() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_sample_bundle(dir.path());
        fs::remove_file(dir.path().join(&manifest.messages[0].rel_path)).unwrap();

        let outcome = verify_bundle(dir.path(), false).unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.missing.len(), 1);
        assert_eq!(outcome.missing[0].rel_path, manifest.messages[0].rel_path);
    }

    #[test]
    fn evidence_manifest_rejects_traversal_rel_path() {
        let mut manifest = sample_manifest_for(&sample_rfc822());
        manifest.messages[0].rel_path = "../escape.eml".to_string();

        let err = validate_manifest(&manifest).unwrap_err();
        assert!(
            err.to_string().contains("parent")
                || err.to_string().contains("messages/<encoded_folder>")
                || err.to_string().contains("must start with messages")
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_rejects_symlinked_evidence_eml_without_following() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let manifest = write_sample_bundle(dir.path());
        let message_path = dir.path().join(&manifest.messages[0].rel_path);
        let outside_message = outside.path().join("outside.eml");
        fs::write(&outside_message, sample_rfc822()).unwrap();
        fs::remove_file(&message_path).unwrap();
        std::os::unix::fs::symlink(&outside_message, &message_path).unwrap();

        let outcome = verify_bundle(dir.path(), false).unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.missing.len(), 1);
        assert_eq!(outcome.missing[0].rel_path, manifest.messages[0].rel_path);
    }

    #[test]
    fn verify_reports_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_sample_bundle(dir.path());
        fs::write(dir.path().join(&manifest.messages[0].rel_path), b"short").unwrap();

        let outcome = verify_bundle(dir.path(), false).unwrap();
        assert!(!outcome.ok);
        assert!(matches!(
            outcome.corrupt.first(),
            Some(EvidenceCorruptFile::SizeMismatch { .. })
        ));
    }

    #[test]
    fn verify_reports_sha_mismatch_for_same_size_payload() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_sample_bundle(dir.path());
        let same_size = vec![b'X'; manifest.messages[0].size as usize];
        fs::write(dir.path().join(&manifest.messages[0].rel_path), same_size).unwrap();

        let outcome = verify_bundle(dir.path(), false).unwrap();
        assert!(!outcome.ok);
        assert!(matches!(
            outcome.corrupt.first(),
            Some(EvidenceCorruptFile::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn verify_strict_fails_unreferenced_extra_eml() {
        let dir = tempfile::tempdir().unwrap();
        write_sample_bundle(dir.path());
        fs::write(
            dir.path()
                .join("messages/%5BGmail%5D%2FAll%20Mail/777-99.eml"),
            b"extra",
        )
        .unwrap();

        let non_strict = verify_bundle(dir.path(), false).unwrap();
        assert!(non_strict.ok);
        assert_eq!(non_strict.extras.len(), 1);

        let strict = verify_bundle(dir.path(), true).unwrap();
        assert!(!strict.ok);
        assert_eq!(strict.extras.len(), 1);
    }

    #[test]
    fn verify_reports_top_level_digest_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        write_sample_bundle(dir.path());
        fs::write(
            dir.path().join("bundle.sha256"),
            format!("{}\n", "0".repeat(64)),
        )
        .unwrap();

        let outcome = verify_bundle(dir.path(), false).unwrap();
        assert!(!outcome.ok);
        assert!(outcome.top_level_digest_mismatch);
    }
}
