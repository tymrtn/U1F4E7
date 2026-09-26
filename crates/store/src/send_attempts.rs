// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Durable send intents, send attempts, receipts, and stale-claim recovery.
//!
//! Three identities, kept apart:
//!
//! - **Intent**: one requested message. A `send`/`reply` request is written
//!   as a `drafts` row carrying an immutable `metadata.send_intent` block
//!   (principal, key hash, payload digest, the row revision it was created
//!   at) before any network work, so a rerun of the same request finds it.
//! - **Attempt**: one claim of a row for transmission, recorded in
//!   `metadata.send_attempt` with its own id, its own Message-ID, the owning
//!   process, and a phase. Finished attempts move to
//!   `metadata.send_attempt_history`.
//! - **Draft revision**: the editable content. An intent only stands for the
//!   row while the row is still at the revision the intent recorded.
//!
//! Attempt phases:
//!
//! - `claimed`: the row is `sending`, no body byte has been written. Safe to
//!   release: the owner must win a compare-and-set to `transmitting` before
//!   it writes the body, so a released claim can never reach the wire.
//! - `transmitting`: committed before the first body byte. From here on the
//!   message may have been accepted, and the row is never released for
//!   another attempt unless the server authoritatively refused it.
//! - `accepted` (row `sent`), `released` (row back to `draft` or parked for
//!   review), `uncertain` (row `delivery_uncertain`).
//!
//! Every transition runs in one `BEGIN IMMEDIATE` transaction that re-reads
//! the row, checks status, lease token, attempt and phase, writes the new
//! state, and appends its receipt to `action_log` (plus lifecycle events).
//! A transition cannot commit without its receipt.
//!
//! Liveness of an attempt's owner comes from an OS file lock the owner holds
//! for the whole attempt (see [`AttemptLock`]). The kernel drops it when the
//! process dies, so a dead owner is detected without trusting PIDs or clocks,
//! and a stopped process still counts as alive.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use base64::Engine;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::db::Database;
use crate::errors::{Result, StoreError};
use crate::event_catalog;
use crate::models::{Draft, DraftStatus};

pub const SEND_INTENT_KEY: &str = "send_intent";
pub const SEND_ATTEMPT_KEY: &str = "send_attempt";
pub const SEND_ATTEMPT_HISTORY_KEY: &str = "send_attempt_history";
/// Metadata keys only this module writes. Generic metadata writes carry the
/// stored values forward and never accept a caller's.
pub const SERVER_OWNED_METADATA_KEYS: &[&str] =
    &[SEND_INTENT_KEY, SEND_ATTEMPT_KEY, SEND_ATTEMPT_HISTORY_KEY];
/// Format version of the intent and attempt blocks.
pub const SEND_RECORD_FORMAT: u64 = 1;
/// Receipt schema written into `action_log.action_taken`.
pub const SEND_RECEIPT_SCHEMA: &str = "envelope.send_receipt.v1";
/// `action_log.action_type` of every send receipt.
pub const SEND_RECEIPT_ACTION: &str = "send";
/// How long an attempt whose owner cannot be checked (another host) may stay
/// `sending` before recovery acts on it: the 10-minute SMTP deadline plus margin.
pub const SEND_LEASE_SECONDS: i64 = 15 * 60;
/// How long a `sent` intent answers an identical request with no explicit key.
/// Unresolved, queued and uncertain intents answer for as long as they exist.
pub const IMPLICIT_SENT_WINDOW_SECONDS: i64 = 15 * 60;
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 200;
const MAX_ATTEMPT_HISTORY: usize = 16;
const LOCK_DIR_NAME: &str = "send-locks";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptPhase {
    Claimed,
    Transmitting,
    Accepted,
    Released,
    Uncertain,
}

impl AttemptPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Transmitting => "transmitting",
            Self::Accepted => "accepted",
            Self::Released => "released",
            Self::Uncertain => "uncertain",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "claimed" => Self::Claimed,
            "transmitting" => Self::Transmitting,
            "accepted" => Self::Accepted,
            "released" => Self::Released,
            "uncertain" => Self::Uncertain,
            _ => return None,
        })
    }
}

/// The process that holds an attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptOwner {
    pub pid: u32,
    pub host: String,
}

impl AttemptOwner {
    pub fn current() -> Self {
        Self {
            pid: std::process::id(),
            host: current_host(),
        }
    }
}

fn current_host() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default()
}

/// Identity of a new attempt, chosen before the claim so the owner can take
/// its lock first and the Message-ID is durable before any network work.
#[derive(Debug, Clone)]
pub struct AttemptStart<'a> {
    pub attempt_id: String,
    pub message_id: &'a str,
    pub surface: &'a str,
    pub agent_id: Option<&'a str>,
    pub owner: AttemptOwner,
}

impl<'a> AttemptStart<'a> {
    pub fn new(message_id: &'a str, surface: &'a str, agent_id: Option<&'a str>) -> Self {
        Self {
            attempt_id: Uuid::new_v4().to_string(),
            message_id,
            surface,
            agent_id,
            owner: AttemptOwner::current(),
        }
    }
}

/// A won claim. `token` is the lease every later transition must present.
#[derive(Debug, Clone)]
pub struct AttemptClaim {
    pub token: String,
    pub attempt_id: String,
    pub message_id: String,
    /// The row as claimed: the snapshot to transmit.
    pub draft: Draft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimMode {
    /// CLI/MCP send now: any `draft` row at the expected revision.
    Immediate,
    /// Outbox sweep: only a row whose `send_after` is due.
    Due,
}

/// Whether an attempt's owner may still act on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
    /// Cannot be checked from here (another host, unreadable lock).
    Unknown,
}

/// Why a claim is being released for another attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseBasis {
    /// The body was never started. Only a `claimed` attempt qualifies.
    NotStarted,
    /// The server answered with a 4xx/5xx reply, including to the body.
    Refused,
}

#[derive(Debug, Clone, Copy)]
pub enum IntentKey<'a> {
    /// A caller-chosen operation key: the hard guarantee. Never expires.
    Explicit(&'a str),
    /// The payload digest: a heuristic for reruns of the identical request.
    Fingerprint,
}

impl IntentKey<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Self::Explicit(_) => "explicit",
            Self::Fingerprint => "fingerprint",
        }
    }
}

/// A requested message, as the caller would transmit it.
pub struct NewSendIntent<'a> {
    pub account_id: &'a str,
    /// Who asked: `local` for the CLI operator, `agent:<id>` for an MCP agent.
    pub principal: &'a str,
    pub agent_id: Option<&'a str>,
    pub surface: &'a str,
    pub key: IntentKey<'a>,
    pub created_by: &'a str,
    pub to: &'a str,
    pub cc: Option<&'a str>,
    pub bcc: Option<&'a str>,
    pub reply_to: Option<&'a str>,
    pub subject: &'a str,
    pub text: Option<&'a str>,
    pub html: Option<&'a str>,
    pub in_reply_to: Option<&'a str>,
    /// Snapshot entries with `data_base64`, as stored on queued drafts.
    pub attachments: &'a [Value],
    /// Caller metadata stored with the row (`from`, `agent_body_text`,
    /// reply threading). Server-owned keys in it are ignored.
    pub metadata: Value,
}

#[derive(Debug, Clone)]
pub enum IntentLookup {
    Created(Draft),
    /// An earlier intent for the same request.
    Existing(Draft),
    /// The explicit key names an intent whose payload differs, or whose row
    /// was edited after the intent was recorded. Nothing may be sent.
    KeyConflict(Draft),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub released: Vec<String>,
    pub parked: Vec<String>,
    pub left_alive: usize,
    /// Attempts resolved because their owner was gone.
    pub resolved_attempts: Vec<String>,
}

/// Validate a caller-supplied idempotency key: bounded, printable ASCII.
/// Keys are opaque; only their hash is stored.
pub fn validate_idempotency_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(StoreError::InvalidIdempotencyKey(format!(
            "must be 1..={MAX_IDEMPOTENCY_KEY_LEN} characters"
        )));
    }
    if !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(StoreError::InvalidIdempotencyKey(
            "must be printable ASCII with no spaces".to_string(),
        ));
    }
    Ok(())
}

// ── Digests ─────────────────────────────────────────────────────────────

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// One parsed mailbox: display name, local part (case kept: only domains
/// are case-insensitive), domain (lowercased).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct MailboxKey {
    name: String,
    local: String,
    domain: String,
}

/// Split an RFC 5322 address list on commas outside quotes and angle brackets.
fn split_address_list(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut angle, mut escaped) = (false, false, false);
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => {
                escaped = true;
                current.push(ch);
            }
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            '<' if !quoted => {
                angle = true;
                current.push(ch);
            }
            '>' if !quoted => {
                angle = false;
                current.push(ch);
            }
            ',' | ';' if !quoted && !angle => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn mailbox_key(entry: &str) -> MailboxKey {
    let (name, addr) = match (entry.rfind('<'), entry.rfind('>')) {
        (Some(open), Some(close)) if open < close => (
            entry[..open].trim().trim_matches('"').trim().to_string(),
            entry[open + 1..close].trim().to_string(),
        ),
        _ => (String::new(), entry.trim().to_string()),
    };
    match addr.rsplit_once('@') {
        Some((local, domain)) => MailboxKey {
            name,
            local: local.to_string(),
            domain: domain.to_lowercase(),
        },
        None => MailboxKey {
            name,
            local: addr,
            domain: String::new(),
        },
    }
}

fn mailbox_keys(value: Option<&str>) -> Vec<MailboxKey> {
    let mut keys: Vec<MailboxKey> = value
        .map(split_address_list)
        .unwrap_or_default()
        .iter()
        .map(|e| mailbox_key(e))
        .collect();
    keys.sort();
    keys
}

/// Every envelope recipient (to, cc, bcc) as a lowercased address, sorted and
/// deduplicated: the form receipts carry.
pub fn recipient_addresses(to: &str, cc: Option<&str>, bcc: Option<&str>) -> Vec<String> {
    let mut all: Vec<String> = [Some(to), cc, bcc]
        .into_iter()
        .flatten()
        .flat_map(split_address_list)
        .map(|e| {
            let key = mailbox_key(&e);
            if key.domain.is_empty() {
                key.local.to_lowercase()
            } else {
                format!("{}@{}", key.local, key.domain).to_lowercase()
            }
        })
        .collect();
    all.sort();
    all.dedup();
    all
}

/// Everything that makes one message distinct, in the fixed field order of
/// `envelope.payload.v1`.
#[derive(Serialize)]
struct PayloadCanon<'a> {
    v: &'static str,
    account_id: &'a str,
    from: Option<&'a str>,
    to: Vec<MailboxKey>,
    cc: Vec<MailboxKey>,
    bcc: Vec<MailboxKey>,
    reply_to: Option<&'a str>,
    in_reply_to: Option<&'a str>,
    references: Vec<String>,
    subject: &'a str,
    text: Option<&'a str>,
    html: Option<&'a str>,
    attachments: Vec<AttachmentCanon>,
}

#[derive(Serialize)]
struct AttachmentCanon {
    filename: String,
    content_type: String,
    sha256: String,
}

fn attachment_canon(entries: &[Value]) -> Result<Vec<AttachmentCanon>> {
    entries
        .iter()
        .map(|entry| {
            let field = |k: &str| entry.get(k).and_then(Value::as_str).unwrap_or("");
            let bytes = match entry.get("data_base64").and_then(Value::as_str) {
                Some(data) => base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| {
                        StoreError::Config(format!(
                            "attachment {} has undecodable bytes: {e}",
                            field("filename")
                        ))
                    })?,
                None => Vec::new(),
            };
            Ok(AttachmentCanon {
                filename: field("filename").to_string(),
                content_type: field("content_type").to_string(),
                sha256: sha256_hex(&bytes),
            })
        })
        .collect()
}

/// Content fields of a message, from a request or a stored row.
struct PayloadFields<'a> {
    account_id: &'a str,
    from: Option<&'a str>,
    to: &'a str,
    cc: Option<&'a str>,
    bcc: Option<&'a str>,
    reply_to: Option<&'a str>,
    in_reply_to: Option<&'a str>,
    references: Vec<String>,
    subject: &'a str,
    text: Option<&'a str>,
    html: Option<&'a str>,
    attachments: &'a [Value],
}

impl<'a> PayloadFields<'a> {
    fn of_draft(draft: &'a Draft) -> Self {
        let meta = draft.metadata.as_ref();
        let from = meta
            .and_then(|m| m.get("from"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|f| !f.is_empty());
        let in_reply_to = meta
            .and_then(|m| m.get("in_reply_to"))
            .and_then(Value::as_str)
            .or(draft.in_reply_to.as_deref());
        let references = meta
            .and_then(|m| m.get("references"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            account_id: &draft.account_id,
            from,
            to: &draft.to_addr,
            cc: draft.cc_addr.as_deref(),
            bcc: draft.bcc_addr.as_deref(),
            reply_to: draft.reply_to.as_deref(),
            in_reply_to,
            references,
            subject: draft.subject.as_deref().unwrap_or(""),
            text: draft.text_content.as_deref(),
            html: draft.html_content.as_deref(),
            attachments: &draft.attachments,
        }
    }

    fn of_intent(spec: &'a NewSendIntent<'a>) -> Self {
        let from = spec
            .metadata
            .get("from")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|f| !f.is_empty());
        let references = spec
            .metadata
            .get("references")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            account_id: spec.account_id,
            from,
            to: spec.to,
            cc: spec.cc,
            bcc: spec.bcc,
            reply_to: spec.reply_to,
            in_reply_to: spec.in_reply_to,
            references,
            subject: spec.subject,
            text: spec.text,
            html: spec.html,
            attachments: spec.attachments,
        }
    }

    /// `envelope.payload.v1`: SHA-256 over every field that changes what is
    /// transmitted. Absent and empty stay distinct; recipients keep their
    /// roles and their local-part case.
    fn payload_sha256(&self) -> Result<String> {
        let canon = PayloadCanon {
            v: "envelope.payload.v1",
            account_id: self.account_id,
            from: self.from,
            to: mailbox_keys(Some(self.to)),
            cc: mailbox_keys(self.cc),
            bcc: mailbox_keys(self.bcc),
            reply_to: self.reply_to,
            in_reply_to: self.in_reply_to,
            references: self.references.clone(),
            subject: self.subject,
            text: self.text,
            html: self.html,
            attachments: attachment_canon(self.attachments)?,
        };
        Ok(sha256_hex(&serde_json::to_vec(&canon)?))
    }

    fn semantic_sha256(&self) -> String {
        semantic_sha256(
            self.subject,
            &recipient_addresses(self.to, self.cc, self.bcc),
            self.text.unwrap_or(""),
        )
    }
}

/// `envelope.semantic.v1`: subject, lowercased recipients and text body, with
/// line endings and trailing whitespace normalized. Message-ID, Date, HTML,
/// attachments, sender and threading are left out, so it matches the same
/// content across resends. It is byte-for-byte the Mailroom bench's
/// `payload_hash` (Python `json.dumps(sort_keys=True)`, ASCII-escaped).
pub fn semantic_sha256(subject: &str, recipients: &[String], text: &str) -> String {
    let unix = text.replace("\r\n", "\n");
    let body = unix
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim_matches('\n');
    let mut lowered: Vec<String> = recipients.iter().map(|r| r.to_lowercase()).collect();
    lowered.sort();
    lowered.dedup();
    let recipients_json = lowered
        .iter()
        .map(|r| python_json_string(r))
        .collect::<Vec<_>>()
        .join(", ");
    let canon = format!(
        "{{\"recipients\": [{recipients_json}], \"subject\": {}, \"text\": {}}}",
        python_json_string(subject.trim()),
        python_json_string(body)
    );
    sha256_hex(canon.as_bytes())
}

/// A JSON string exactly as Python's `json.dumps` writes it with the default
/// `ensure_ascii=True`.
fn python_json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn intent_key_sha256(principal: &str, key: IntentKey<'_>, payload_sha256: &str) -> String {
    match key {
        IntentKey::Explicit(k) => sha256_hex(format!("explicit\0{principal}\0{k}").as_bytes()),
        IntentKey::Fingerprint => {
            sha256_hex(format!("fingerprint\0{principal}\0{payload_sha256}").as_bytes())
        }
    }
}

// ── Owner locks ─────────────────────────────────────────────────────────

/// The OS lock that marks this process as the live owner of one attempt.
///
/// Taken before the claim commits and held until the attempt reaches a
/// terminal state. The kernel releases it when the process exits for any
/// reason, which is what [`owner_liveness`] observes.
#[derive(Debug)]
pub struct AttemptLock {
    file: Option<File>,
    path: PathBuf,
}

impl AttemptLock {
    pub fn acquire(dir: &Path, attempt_id: &str) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| {
            StoreError::Config(format!(
                "cannot create send lock directory {}: {e}",
                dir.display()
            ))
        })?;
        let path = dir.join(format!("{attempt_id}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| {
                StoreError::Config(format!("cannot create send lock {}: {e}", path.display()))
            })?;
        file.try_lock().map_err(|e| {
            StoreError::Config(format!("cannot lock send lock {}: {e}", path.display()))
        })?;
        Ok(Self {
            file: Some(file),
            path,
        })
    }
}

impl Drop for AttemptLock {
    fn drop(&mut self) {
        // Remove first, then close (which unlocks). A prober that opened the
        // file before removal sees the lock free once we close: the owner is
        // gone, which is true.
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("could not remove send lock {}: {e}", self.path.display());
        }
        drop(self.file.take());
    }
}

/// Whether the owner of `attempt_id` still holds its lock.
pub fn owner_liveness(lock_dir: Option<&Path>, owner: &AttemptOwner, attempt_id: &str) -> Liveness {
    let Some(dir) = lock_dir else {
        return Liveness::Unknown;
    };
    if owner.host != current_host() {
        return Liveness::Unknown;
    }
    let path = dir.join(format!("{attempt_id}.lock"));
    match File::open(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Liveness::Dead,
        Err(_) => Liveness::Unknown,
        Ok(file) => match file.try_lock() {
            Ok(()) => Liveness::Dead,
            Err(std::fs::TryLockError::WouldBlock) => Liveness::Alive,
            Err(std::fs::TryLockError::Error(_)) => Liveness::Unknown,
        },
    }
}

// ── Transactions ────────────────────────────────────────────────────────

/// `BEGIN IMMEDIATE` … `COMMIT`, rolled back on drop. Taking the write lock
/// up front means the read that validates a transition and the write that
/// applies it cannot be interleaved with another writer. A lock that stays
/// busy past `busy_timeout` is an error: callers fail closed.
pub(crate) struct ImmediateTx<'a> {
    db: &'a Database,
    open: bool,
}

impl<'a> ImmediateTx<'a> {
    pub(crate) fn begin(db: &'a Database) -> Result<Self> {
        db.conn().execute_batch("BEGIN IMMEDIATE")?;
        Ok(Self { db, open: true })
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        self.db.conn().execute_batch("COMMIT")?;
        self.open = false;
        Ok(())
    }
}

impl Drop for ImmediateTx<'_> {
    fn drop(&mut self) {
        if self.open
            && let Err(e) = self.db.conn().execute_batch("ROLLBACK")
        {
            tracing::error!("send-attempt transaction rollback failed: {e}");
        }
    }
}

// ── Row views ───────────────────────────────────────────────────────────

/// A draft row plus the lease columns `Draft` does not expose.
struct LeaseRow {
    draft: Draft,
    token: Option<String>,
}

impl LeaseRow {
    fn metadata(&self) -> Map<String, Value> {
        match &self.draft.metadata {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        }
    }

    fn intent(&self) -> Option<&Value> {
        self.draft
            .metadata
            .as_ref()?
            .get(SEND_INTENT_KEY)
            .filter(|v| v.is_object())
    }

    /// The current attempt, only when it is the one the stored lease token
    /// belongs to (or no token is held). A row claimed by an older binary
    /// keeps a stale block that must not be trusted.
    fn attempt(&self) -> Option<&Value> {
        let attempt = self
            .draft
            .metadata
            .as_ref()?
            .get(SEND_ATTEMPT_KEY)
            .filter(|v| v.is_object())?;
        match (
            &self.token,
            attempt.get("lease_sha256").and_then(Value::as_str),
        ) {
            (Some(token), Some(hash)) if sha256_hex(token.as_bytes()) == hash => Some(attempt),
            (Some(_), _) => None,
            (None, _) => Some(attempt),
        }
    }

    fn phase(&self) -> Option<AttemptPhase> {
        self.attempt()?
            .get("phase")
            .and_then(Value::as_str)
            .and_then(AttemptPhase::parse)
    }
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

/// The receipt-facing description of one transition.
pub(crate) struct Transition<'a> {
    pub(crate) to_status: &'a str,
    pub(crate) phase: AttemptPhase,
    pub(crate) reason: &'a str,
    pub(crate) retryable: Option<bool>,
    pub(crate) evidence: Option<Value>,
    /// Who performed it; `None` means the attempt's own agent, if any.
    pub(crate) agent_id: Option<&'a str>,
}

impl Database {
    /// Where attempt owners keep their locks: next to the database file.
    /// `None` for an in-memory database.
    pub fn send_lock_dir(&self) -> Option<PathBuf> {
        let path = self.conn().path()?;
        if path.is_empty() {
            return None;
        }
        Path::new(path).parent().map(|p| p.join(LOCK_DIR_NAME))
    }

    fn lease_row(&self, id: &str) -> Result<Option<LeaseRow>> {
        let Some(draft) = self.get_draft(id)? else {
            return Ok(None);
        };
        let token: Option<String> = self
            .conn()
            .query_row(
                "SELECT operation_token FROM drafts WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(Some(LeaseRow { draft, token }))
    }

    // ── Intents ─────────────────────────────────────────────────────────

    /// Find the intent for this request, or record a new one.
    ///
    /// The lookup and the insert share one write transaction, so two
    /// identical requests racing each other end with one row. A new intent is
    /// a `draft` row carrying the full content and attachment bytes; the
    /// caller then claims or queues it.
    ///
    /// Matching is scoped to the account and the principal. An explicit key
    /// matches its intent for as long as the row exists, whatever its status.
    /// A fingerprint matches any intent that is not discarded, except a `sent`
    /// one older than [`IMPLICIT_SENT_WINDOW_SECONDS`], and never a row that
    /// was edited after the intent was recorded.
    pub fn find_or_create_send_intent(&self, spec: &NewSendIntent<'_>) -> Result<IntentLookup> {
        if let IntentKey::Explicit(key) = spec.key {
            validate_idempotency_key(key)?;
        }
        let fields = PayloadFields::of_intent(spec);
        let payload_sha256 = fields.payload_sha256()?;
        let semantic_sha256 = fields.semantic_sha256();
        let key_sha256 = intent_key_sha256(spec.principal, spec.key, &payload_sha256);

        let tx = ImmediateTx::begin(self)?;
        if let Some(found) = self.match_intent(spec, &key_sha256, &payload_sha256)? {
            return Ok(found);
        }

        let id = Uuid::new_v4().to_string();
        let mut metadata = match spec.metadata.clone() {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        for key in SERVER_OWNED_METADATA_KEYS {
            metadata.remove(*key);
        }
        metadata.insert(
            SEND_INTENT_KEY.to_string(),
            json!({
                "format": SEND_RECORD_FORMAT,
                "surface": spec.surface,
                "principal": spec.principal,
                "agent_id": spec.agent_id,
                "key_kind": spec.key.kind(),
                "key_sha256": key_sha256,
                "payload_sha256": payload_sha256,
                "semantic_sha256": semantic_sha256,
                "revision": 0,
                "created_at": now_rfc3339(),
            }),
        );
        self.conn().execute(
            "INSERT INTO drafts (id, account_id, to_addr, cc_addr, bcc_addr, reply_to, subject,
                 text_content, html_content, in_reply_to, attachments, metadata, created_by,
                 revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 0)",
            params![
                id,
                spec.account_id,
                spec.to,
                spec.cc,
                spec.bcc,
                spec.reply_to,
                spec.subject,
                spec.text,
                spec.html,
                spec.in_reply_to,
                serde_json::to_string(spec.attachments)?,
                serde_json::to_string(&Value::Object(metadata))?,
                spec.created_by,
            ],
        )?;
        let draft = self
            .get_draft(&id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.clone()))?;
        tx.commit()?;
        Ok(IntentLookup::Created(draft))
    }

    /// The intent an earlier identical request recorded, without recording
    /// one: lets a caller decide (and run the Governor gate) before anything
    /// is written. [`Self::find_or_create_send_intent`] repeats the lookup
    /// inside its write transaction.
    pub fn lookup_send_intent(&self, spec: &NewSendIntent<'_>) -> Result<Option<IntentLookup>> {
        if let IntentKey::Explicit(key) = spec.key {
            validate_idempotency_key(key)?;
        }
        let payload_sha256 = PayloadFields::of_intent(spec).payload_sha256()?;
        let key_sha256 = intent_key_sha256(spec.principal, spec.key, &payload_sha256);
        self.match_intent(spec, &key_sha256, &payload_sha256)
    }

    fn match_intent(
        &self,
        spec: &NewSendIntent<'_>,
        key_sha256: &str,
        payload_sha256: &str,
    ) -> Result<Option<IntentLookup>> {
        let ids: Vec<String> = {
            let mut stmt = self.conn().prepare(
                "SELECT id FROM drafts
                 WHERE account_id = ?1
                   AND json_valid(metadata)
                   AND json_extract(metadata, '$.send_intent.key_sha256') = ?2
                   AND json_extract(metadata, '$.send_intent.principal') = ?3
                 ORDER BY created_at DESC, rowid DESC",
            )?;
            stmt.query_map(
                params![spec.account_id, key_sha256, spec.principal],
                |row| row.get(0),
            )?
            .collect::<std::result::Result<_, _>>()?
        };
        for id in ids {
            let Some(draft) = self.get_draft(&id)? else {
                continue;
            };
            let intent = draft
                .metadata
                .as_ref()
                .and_then(|m| m.get(SEND_INTENT_KEY))
                .cloned()
                .unwrap_or(Value::Null);
            let unedited = intent.get("revision").and_then(Value::as_i64) == Some(draft.revision);
            let same_payload =
                intent.get("payload_sha256").and_then(Value::as_str) == Some(payload_sha256);
            match spec.key {
                IntentKey::Explicit(_) => {
                    return Ok(Some(if unedited && same_payload {
                        IntentLookup::Existing(draft)
                    } else {
                        IntentLookup::KeyConflict(draft)
                    }));
                }
                IntentKey::Fingerprint => {
                    if !unedited || draft.status == DraftStatus::Discarded {
                        continue;
                    }
                    if draft.status == DraftStatus::Sent && !sent_within_window(&draft) {
                        continue;
                    }
                    return Ok(Some(IntentLookup::Existing(draft)));
                }
            }
        }
        Ok(None)
    }

    // ── Attempts ────────────────────────────────────────────────────────

    /// Claim a `draft` row for one transmission attempt.
    ///
    /// The row moves to `sending` with a fresh lease token and a new
    /// `send_attempt` block in phase `claimed`; any earlier attempt moves to
    /// the history. Returns `None` when the claim is lost: another claim, a
    /// stale revision, a non-`draft` status, or (for [`ClaimMode::Due`]) a
    /// row that is not due. The caller must hold [`AttemptLock`] for
    /// `start.attempt_id` before calling.
    pub fn claim_send_attempt(
        &self,
        id: &str,
        expected_revision: i64,
        mode: ClaimMode,
        start: &AttemptStart<'_>,
    ) -> Result<Option<AttemptClaim>> {
        let tx = ImmediateTx::begin(self)?;
        let Some(row) = self.lease_row(id)? else {
            return Ok(None);
        };
        if row.draft.status != DraftStatus::Draft || row.draft.revision != expected_revision {
            return Ok(None);
        }
        let from_status = display_status(&row.draft);
        let token = Uuid::new_v4().to_string();
        let mut metadata = row.metadata();
        if let Some(previous) = metadata.remove(SEND_ATTEMPT_KEY) {
            push_history(&mut metadata, previous);
        }
        metadata.insert(
            SEND_ATTEMPT_KEY.to_string(),
            json!({
                "format": SEND_RECORD_FORMAT,
                "attempt_id": start.attempt_id,
                "message_id": start.message_id,
                "phase": AttemptPhase::Claimed.as_str(),
                "owner": start.owner,
                "claimed_at": now_rfc3339(),
                "surface": start.surface,
                "agent_id": start.agent_id,
                "lease_sha256": sha256_hex(token.as_bytes()),
                "seq": 0,
            }),
        );
        let due_clause = match mode {
            ClaimMode::Immediate => "",
            ClaimMode::Due => {
                "AND send_after IS NOT NULL AND datetime(send_after) <= datetime('now')"
            }
        };
        let rows = self.conn().execute(
            &format!(
                "UPDATE drafts SET status = 'sending', operation_token = ?1, metadata = ?2,
                    updated_at = datetime('now')
                 WHERE id = ?3 AND status = 'draft' AND revision = ?4 {due_clause}"
            ),
            params![
                token,
                serde_json::to_string(&Value::Object(metadata))?,
                id,
                expected_revision
            ],
        )?;
        if rows != 1 {
            return Ok(None);
        }
        let claimed = self
            .lease_row(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        self.write_receipt(
            &claimed,
            from_status,
            &Transition {
                to_status: "sending",
                phase: AttemptPhase::Claimed,
                reason: "claimed",
                retryable: None,
                evidence: None,
                agent_id: start.agent_id,
            },
            false,
        )?;
        tx.commit()?;
        Ok(Some(AttemptClaim {
            token,
            attempt_id: start.attempt_id.clone(),
            message_id: start.message_id.to_string(),
            draft: claimed.draft,
        }))
    }

    /// Move an attempt from `claimed` to `transmitting`. The caller writes
    /// the first body byte only after this returns `Ok(true)`. `Ok(false)`
    /// means the claim was released or taken over: stop without transmitting.
    pub fn begin_transmitting(&self, id: &str, token: &str) -> Result<bool> {
        let tx = ImmediateTx::begin(self)?;
        let Some(row) = self.lease_row(id)? else {
            return Ok(false);
        };
        if row.draft.status != DraftStatus::Sending
            || row.token.as_deref() != Some(token)
            || row.phase() != Some(AttemptPhase::Claimed)
        {
            return Ok(false);
        }
        let agent_id = attempt_agent(&row);
        let mut metadata = row.metadata();
        if let Some(Value::Object(attempt)) = metadata.get_mut(SEND_ATTEMPT_KEY) {
            attempt.insert("phase".into(), json!(AttemptPhase::Transmitting.as_str()));
            attempt.insert("transmitting_at".into(), json!(now_rfc3339()));
        }
        let rows = self.conn().execute(
            "UPDATE drafts SET metadata = ?1, updated_at = datetime('now')
             WHERE id = ?2 AND status = 'sending' AND operation_token = ?3",
            params![serde_json::to_string(&Value::Object(metadata))?, id, token],
        )?;
        if rows != 1 {
            return Ok(false);
        }
        let updated = self
            .lease_row(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        self.write_receipt(
            &updated,
            "sending",
            &Transition {
                to_status: "sending",
                phase: AttemptPhase::Transmitting,
                reason: "body_started",
                retryable: None,
                evidence: None,
                agent_id: agent_id.as_deref(),
            },
            false,
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Record the server's acceptance of an attempt: the row becomes `sent`,
    /// with its receipt and the `send_completed` event in the same commit.
    ///
    /// Accepted from the lease holder of a `sending` row, and from the same
    /// attempt after recovery parked it `delivery_uncertain` (late evidence:
    /// the token still proves which attempt this is). Anything else is
    /// [`StoreError::DraftNotEditable`].
    pub fn finish_attempt_sent(
        &self,
        id: &str,
        token: &str,
        message_id: &str,
        evidence: Value,
    ) -> Result<()> {
        let tx = ImmediateTx::begin(self)?;
        let row = self
            .lease_row(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        let token_hash = sha256_hex(token.as_bytes());
        let stored_hash = row
            .draft
            .metadata
            .as_ref()
            .and_then(|m| m.get(SEND_ATTEMPT_KEY))
            .and_then(|a| a.get("lease_sha256"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let (from_status, holder) = match row.draft.status {
            DraftStatus::Sending if row.token.as_deref() == Some(token) => ("sending", true),
            DraftStatus::DeliveryUncertain if stored_hash.as_deref() == Some(&token_hash) => {
                ("delivery_uncertain", false)
            }
            _ => {
                return Err(StoreError::DraftNotEditable(format!(
                    "{} — only the holder of the `sending` lease can mark this draft sent",
                    row.draft.status.as_str()
                )));
            }
        };
        let agent_id = attempt_agent(&row);
        let mut metadata = row.metadata();
        if let Some(Value::Object(attempt)) = metadata.get_mut(SEND_ATTEMPT_KEY) {
            attempt.insert("phase".into(), json!(AttemptPhase::Accepted.as_str()));
            attempt.insert(
                "outcome".into(),
                json!({"status": "sent", "at": now_rfc3339(), "evidence": evidence}),
            );
        }
        let rows = self.conn().execute(
            "UPDATE drafts SET status = 'sent', message_id = ?1, operation_token = NULL,
                send_after = NULL, imap_uid = NULL, metadata = ?2,
                sent_at = datetime('now'), updated_at = datetime('now')
             WHERE id = ?3 AND status = ?4 AND COALESCE(operation_token, '') = ?5",
            params![
                message_id,
                serde_json::to_string(&Value::Object(metadata))?,
                id,
                from_status,
                if holder { token } else { "" }
            ],
        )?;
        if rows != 1 {
            return Err(StoreError::DraftModifiedConcurrently(id.to_string()));
        }
        let sent = self
            .lease_row(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        self.write_receipt(
            &sent,
            from_status,
            &Transition {
                to_status: "sent",
                phase: AttemptPhase::Accepted,
                reason: if holder {
                    "accepted"
                } else {
                    "late_acceptance"
                },
                retryable: Some(false),
                evidence: Some(evidence),
                agent_id: agent_id.as_deref(),
            },
            false,
        )?;
        self.emit_catalog_event_for_message(
            &sent.draft.account_id,
            event_catalog::SEND_COMPLETED,
            Some(json!({
                "draft_id": id,
                "message_id": message_id,
                "attempt_id": attempt_field(&sent, "attempt_id"),
            })),
            agent_id.as_deref(),
            Some(message_id),
        )?;
        tx.commit()?;

        // The send is durable from the commit above; the suggestion cache is
        // maintenance. Failing the send here would park delivered mail.
        if let Err(e) = self.record_sent_draft_recipients(id) {
            tracing::warn!(
                draft_id = %id,
                "draft was sent, but its recipients could not be folded into the \
                 address history: {e} — they will appear in compose autocomplete \
                 after the next reconcile"
            );
        }
        Ok(())
    }

    /// Return a claimed row to `to` for a later attempt, or park it for a
    /// person (`blocked`/`pending_review`, with `send_block` explaining why).
    ///
    /// [`ReleaseBasis::NotStarted`] accepts only a `claimed` attempt: once the
    /// body may have started, only an authoritative server refusal
    /// ([`ReleaseBasis::Refused`]) proves nothing was accepted. Returns
    /// `Ok(false)` and writes nothing when the lease, attempt or phase does
    /// not match.
    #[allow(clippy::too_many_arguments)]
    pub fn release_attempt(
        &self,
        id: &str,
        token: &str,
        to: DraftStatus,
        basis: ReleaseBasis,
        reason: &str,
        evidence: Option<Value>,
        send_block: Option<&Value>,
    ) -> Result<bool> {
        let keep_schedule = to == DraftStatus::Draft;
        let block = send_block.map(serde_json::to_string).transpose()?;
        self.lease_transition(
            id,
            token,
            basis,
            Transition {
                to_status: to.as_str(),
                phase: AttemptPhase::Released,
                reason,
                retryable: Some(to == DraftStatus::Draft),
                evidence,
                agent_id: None,
            },
            |metadata| {
                Ok(self.conn().execute(
                    "UPDATE drafts SET status = ?1, operation_token = NULL,
                        send_after = CASE WHEN ?2 THEN send_after ELSE NULL END,
                        metadata = CASE WHEN ?3 IS NULL THEN json(?4)
                            ELSE json_set(json(?4), '$.send_block', json(?3)) END,
                        updated_at = datetime('now')
                     WHERE id = ?5 AND status = 'sending' AND operation_token = ?6",
                    params![to.as_str(), keep_schedule, block, metadata, id, token],
                )?)
            },
        )
    }

    /// Park an attempt whose outcome is unknown as `delivery_uncertain`: never
    /// due, never claimable, never re-sent. Parking is always safe, so any
    /// phase qualifies; only the lease is checked.
    pub fn park_attempt_uncertain(
        &self,
        id: &str,
        token: &str,
        reason: &str,
        evidence: Option<Value>,
    ) -> Result<bool> {
        let tx = ImmediateTx::begin(self)?;
        let Some(row) = self.lease_row(id)? else {
            return Ok(false);
        };
        if row.draft.status != DraftStatus::Sending || row.token.as_deref() != Some(token) {
            return Ok(false);
        }
        if !self.park_in_tx(&row, token, reason, evidence.unwrap_or(Value::Null))? {
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
    }

    /// Shared body of every lease-holder transition out of `sending` other
    /// than acceptance. `apply` runs the specific UPDATE with the new
    /// metadata JSON and must guard on `status = 'sending'` and the token.
    pub(crate) fn lease_transition(
        &self,
        id: &str,
        token: &str,
        basis: ReleaseBasis,
        transition: Transition<'_>,
        apply: impl FnOnce(&str) -> Result<usize>,
    ) -> Result<bool> {
        let tx = ImmediateTx::begin(self)?;
        let Some(row) = self.lease_row(id)? else {
            return Ok(false);
        };
        if !self.lease_transition_in_tx(&row, token, basis, &transition, apply)? {
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
    }

    fn lease_transition_in_tx(
        &self,
        row: &LeaseRow,
        token: &str,
        basis: ReleaseBasis,
        transition: &Transition<'_>,
        apply: impl FnOnce(&str) -> Result<usize>,
    ) -> Result<bool> {
        if row.draft.status != DraftStatus::Sending || row.token.as_deref() != Some(token) {
            return Ok(false);
        }
        let phase = row.phase();
        let permitted = match (basis, phase) {
            // A row claimed before attempts were recorded has no phase.
            (_, None) => basis == ReleaseBasis::NotStarted || row.attempt().is_none(),
            (ReleaseBasis::NotStarted, Some(p)) => p == AttemptPhase::Claimed,
            (ReleaseBasis::Refused, Some(p)) => {
                matches!(p, AttemptPhase::Claimed | AttemptPhase::Transmitting)
            }
        };
        if !permitted {
            tracing::warn!(
                "draft {}: refusing to move a `{}` attempt to `{}` ({})",
                row.draft.id,
                phase.map(AttemptPhase::as_str).unwrap_or("unknown"),
                transition.to_status,
                transition.reason
            );
            return Ok(false);
        }
        let agent_id = transition
            .agent_id
            .map(str::to_string)
            .or_else(|| attempt_agent(row));
        let mut metadata = row.metadata();
        if let Some(Value::Object(attempt)) = metadata.get_mut(SEND_ATTEMPT_KEY) {
            attempt.insert("phase".into(), json!(transition.phase.as_str()));
            attempt.insert(
                "outcome".into(),
                json!({
                    "status": transition.to_status,
                    "reason": transition.reason,
                    "retryable": transition.retryable,
                    "at": now_rfc3339(),
                    "evidence": transition.evidence,
                }),
            );
        }
        let rows = apply(&serde_json::to_string(&Value::Object(metadata))?)?;
        if rows != 1 {
            return Ok(false);
        }
        let after = self
            .lease_row(&row.draft.id)?
            .ok_or_else(|| StoreError::DraftNotFound(row.draft.id.clone()))?;
        self.write_receipt(
            &after,
            "sending",
            &Transition {
                agent_id: agent_id.as_deref(),
                evidence: transition.evidence.clone(),
                ..*transition
            },
            false,
        )?;
        Ok(true)
    }

    // ── Recovery ────────────────────────────────────────────────────────

    /// Resolve `sending` rows whose owner is gone.
    ///
    /// - Owner alive: left alone, however old.
    /// - Owner dead, phase `claimed`: released (a scheduled row keeps its
    ///   `send_after` and comes due again). Nothing reached the wire.
    /// - Owner dead, any later phase: parked `delivery_uncertain`.
    /// - Owner unknown (another host): the same two outcomes, but only after
    ///   [`SEND_LEASE_SECONDS`] from the claim.
    /// - Rows claimed by a binary that kept no attempt record: parked after
    ///   the lease, measured from the claim (`updated_at`: claimed rows
    ///   cannot be edited).
    ///
    /// Each row is re-read and decided inside its own write transaction, so a
    /// worker that advances its phase concurrently wins or loses cleanly.
    pub fn reconcile_stale_sending(
        &self,
        now: DateTime<Utc>,
        liveness: &dyn Fn(&AttemptOwner, &str) -> Liveness,
    ) -> Result<ReconcileReport> {
        let ids: Vec<String> = {
            let mut stmt = self
                .conn()
                .prepare("SELECT id FROM drafts WHERE status = 'sending' ORDER BY updated_at")?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?
        };
        let mut report = ReconcileReport::default();
        for id in ids {
            let tx = ImmediateTx::begin(self)?;
            let Some(row) = self.lease_row(&id)? else {
                continue;
            };
            let Some(token) = row.token.clone() else {
                continue;
            };
            if row.draft.status != DraftStatus::Sending {
                continue;
            }
            let decision = match row.attempt() {
                Some(attempt) => {
                    let owner: Option<AttemptOwner> = attempt
                        .get("owner")
                        .cloned()
                        .and_then(|o| serde_json::from_value(o).ok());
                    let attempt_id = attempt
                        .get("attempt_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let live = owner
                        .as_ref()
                        .map(|o| liveness(o, attempt_id))
                        .unwrap_or(Liveness::Unknown);
                    let claimed_at = attempt
                        .get("claimed_at")
                        .and_then(Value::as_str)
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|t| t.with_timezone(&Utc));
                    let expired =
                        claimed_at.is_none_or(|t| (now - t).num_seconds() >= SEND_LEASE_SECONDS);
                    let claimed = row.phase() == Some(AttemptPhase::Claimed);
                    match live {
                        Liveness::Alive => None,
                        Liveness::Dead => Some((claimed, "owner_dead", live)),
                        Liveness::Unknown if expired => Some((claimed, "lease_expired", live)),
                        Liveness::Unknown => None,
                    }
                }
                None => {
                    let claimed_at = parse_sqlite_time(&row.draft.updated_at);
                    let expired =
                        claimed_at.is_none_or(|t| (now - t).num_seconds() >= SEND_LEASE_SECONDS);
                    expired.then_some((false, "legacy_lease_expired", Liveness::Unknown))
                }
            };
            let Some((release, reason, live)) = decision else {
                report.left_alive += 1;
                continue;
            };
            let evidence = json!({
                "kind": "reconciled",
                "liveness": match live {
                    Liveness::Alive => "alive",
                    Liveness::Dead => "dead",
                    Liveness::Unknown => "unknown",
                },
                "owner": row.attempt().and_then(|a| a.get("owner")).cloned(),
                "claimed_at": row.attempt().and_then(|a| a.get("claimed_at")).cloned(),
                "phase": row.phase().map(AttemptPhase::as_str),
                "at": now.to_rfc3339(),
            });
            let done = if release {
                self.lease_transition_in_tx(
                    &row,
                    &token,
                    ReleaseBasis::NotStarted,
                    &Transition {
                        to_status: "draft",
                        phase: AttemptPhase::Released,
                        reason,
                        retryable: Some(true),
                        evidence: Some(evidence),
                        agent_id: None,
                    },
                    |metadata| {
                        Ok(self.conn().execute(
                            "UPDATE drafts SET status = 'draft', operation_token = NULL,
                                metadata = json(?1), updated_at = datetime('now')
                             WHERE id = ?2 AND status = 'sending' AND operation_token = ?3",
                            params![metadata, id, token],
                        )?)
                    },
                )?
            } else {
                self.park_in_tx(&row, &token, reason, evidence)?
            };
            if done {
                tx.commit()?;
                if let Some(attempt_id) = row
                    .attempt()
                    .and_then(|a| a.get("attempt_id"))
                    .and_then(Value::as_str)
                {
                    report.resolved_attempts.push(attempt_id.to_string());
                }
                if release {
                    report.released.push(id);
                } else {
                    report.parked.push(id);
                }
            }
        }
        Ok(report)
    }

    fn park_in_tx(
        &self,
        row: &LeaseRow,
        token: &str,
        reason: &str,
        evidence: Value,
    ) -> Result<bool> {
        let agent_id = attempt_agent(row);
        let mut metadata = row.metadata();
        if let Some(attempt) = row.attempt().cloned() {
            let mut attempt = attempt;
            if let Value::Object(map) = &mut attempt {
                map.insert("phase".into(), json!(AttemptPhase::Uncertain.as_str()));
                map.insert(
                    "outcome".into(),
                    json!({
                        "status": "delivery_uncertain",
                        "reason": reason,
                        "retryable": false,
                        "at": now_rfc3339(),
                        "evidence": evidence,
                    }),
                );
            }
            metadata.insert(SEND_ATTEMPT_KEY.to_string(), attempt);
        }
        let rows = self.conn().execute(
            "UPDATE drafts SET status = 'delivery_uncertain', send_after = NULL,
                operation_token = NULL, metadata = json(?1), updated_at = datetime('now')
             WHERE id = ?2 AND status = 'sending' AND operation_token = ?3",
            params![
                serde_json::to_string(&Value::Object(metadata))?,
                row.draft.id,
                token
            ],
        )?;
        if rows != 1 {
            return Ok(false);
        }
        let after = self
            .lease_row(&row.draft.id)?
            .ok_or_else(|| StoreError::DraftNotFound(row.draft.id.clone()))?;
        self.write_receipt(
            &after,
            "sending",
            &Transition {
                to_status: "delivery_uncertain",
                phase: AttemptPhase::Uncertain,
                reason,
                retryable: Some(false),
                evidence: (!evidence.is_null()).then_some(evidence),
                agent_id: agent_id.as_deref(),
            },
            false,
        )?;
        Ok(true)
    }

    /// [`Self::reconcile_stale_sending`] with this database's owner locks and
    /// the current time.
    pub fn reconcile_stale_sending_now(&self) -> Result<ReconcileReport> {
        let lock_dir = self.send_lock_dir();
        let report = self.reconcile_stale_sending(Utc::now(), &|owner, attempt_id| {
            owner_liveness(lock_dir.as_deref(), owner, attempt_id)
        })?;
        // A killed owner leaves its (unlocked) lock file behind. Its attempt
        // is resolved now and will never be resumed, so the file can go.
        if let Some(dir) = &lock_dir {
            for attempt_id in &report.resolved_attempts {
                let path = dir.join(format!("{attempt_id}.lock"));
                if let Err(e) = std::fs::remove_file(&path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!("could not remove stale send lock {}: {e}", path.display());
                }
            }
        }
        Ok(report)
    }

    // ── Receipts ────────────────────────────────────────────────────────

    /// Record that a request was answered from an earlier intent without a
    /// new transmission. The receipt carries the underlying outcome, marked
    /// as a replay, and the replaying actor.
    pub fn record_send_replay(
        &self,
        id: &str,
        agent_id: Option<&str>,
        surface: &str,
    ) -> Result<()> {
        let tx = ImmediateTx::begin(self)?;
        let row = self
            .lease_row(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        let status = row.draft.status.as_str().to_string();
        self.write_receipt(
            &row,
            &status,
            &Transition {
                to_status: &status,
                phase: row.phase().unwrap_or(AttemptPhase::Released),
                reason: surface,
                retryable: None,
                evidence: None,
                agent_id,
            },
            true,
        )?;
        tx.commit()
    }

    /// Record a `queued` receipt for a row the outbox now holds. Runs inside
    /// the queue transition's transaction.
    pub(crate) fn write_queued_receipt(
        &self,
        id: &str,
        from_status: &str,
        surface: &str,
        agent_id: Option<&str>,
    ) -> Result<()> {
        let row = self
            .lease_row(id)?
            .ok_or_else(|| StoreError::DraftNotFound(id.to_string()))?;
        self.write_receipt(
            &row,
            from_status,
            &Transition {
                to_status: "queued",
                phase: AttemptPhase::Released,
                reason: surface,
                retryable: None,
                evidence: None,
                agent_id,
            },
            false,
        )
    }

    /// Append one receipt to `action_log` and, for an agent actor, the
    /// `agent_action` event. Recipients stay in the local receipt only; the
    /// event carries an allowlist of identifiers and a count.
    fn write_receipt(
        &self,
        row: &LeaseRow,
        from_status: &str,
        transition: &Transition<'_>,
        replay: bool,
    ) -> Result<()> {
        let draft = &row.draft;
        let fields = PayloadFields::of_draft(draft);
        let intent = row.intent();
        let recipients = recipient_addresses(
            &draft.to_addr,
            draft.cc_addr.as_deref(),
            draft.bcc_addr.as_deref(),
        );
        let payload_sha256 = fields.payload_sha256()?;
        let semantic_sha256 = fields.semantic_sha256();
        let attempt = draft
            .metadata
            .as_ref()
            .and_then(|m| m.get(SEND_ATTEMPT_KEY))
            .filter(|a| a.is_object());
        let seq = attempt
            .and_then(|a| a.get("seq"))
            .and_then(Value::as_u64)
            .map(|s| s + 1);
        let surface = attempt
            .and_then(|a| a.get("surface"))
            .or_else(|| intent.and_then(|i| i.get("surface")))
            .and_then(Value::as_str);
        let message_id = match transition.to_status {
            "sent" => draft.message_id.clone(),
            _ => attempt
                .and_then(|a| a.get("message_id"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    (draft.status == DraftStatus::Sent)
                        .then(|| draft.message_id.clone())
                        .flatten()
                }),
        };
        let attempt_id = attempt
            .and_then(|a| a.get("attempt_id"))
            .and_then(Value::as_str);
        // The row's status after the transition, in `draft show` terms. A
        // replay reports the status the earlier request left.
        let status = display_status(draft);
        let taken = json!({
            "receipt": SEND_RECEIPT_SCHEMA,
            "surface": surface,
            "attempt_id": attempt_id,
            "seq": seq,
            "from": from_status,
            "to": status,
            "phase": transition.phase.as_str(),
            "reason": transition.reason,
            "replay": replay,
            "retryable": transition.retryable,
            "recipients": recipients,
            "recipient_count": recipients.len(),
            "payload_sha256": payload_sha256,
            "semantic_sha256": semantic_sha256,
            "key_kind": intent.and_then(|i| i.get("key_kind")).cloned(),
            "evidence": transition.evidence,
        });
        self.conn().execute(
            "INSERT INTO action_log (
                id, account_id, action_type, confidence, justification, action_taken,
                message_id, draft_id, event_id, action_status, agent_id
             ) VALUES (?1, ?2, ?3, 1.0, ?4, ?5, ?6, ?7, NULL, ?8, ?9)",
            params![
                Uuid::new_v4().to_string(),
                draft.account_id,
                SEND_RECEIPT_ACTION,
                if replay {
                    "send replay"
                } else {
                    "send receipt"
                },
                taken.to_string(),
                message_id,
                draft.id,
                status,
                transition.agent_id,
            ],
        )?;
        if let Some(seq) = seq
            && !replay
        {
            self.conn().execute(
                "UPDATE drafts SET metadata = json_set(metadata, '$.send_attempt.seq', ?1)
                 WHERE id = ?2 AND json_valid(metadata)",
                params![seq as i64, draft.id],
            )?;
        }
        if let Some(agent_id) = transition.agent_id {
            self.emit_catalog_event_for_message(
                &draft.account_id,
                event_catalog::AGENT_ACTION,
                Some(json!({
                    "action_type": SEND_RECEIPT_ACTION,
                    "status": status,
                    "draft_id": draft.id,
                    "attempt_id": attempt_id,
                    "message_id": message_id,
                    "recipient_count": recipients.len(),
                    "surface": surface,
                    "replay": replay,
                })),
                Some(agent_id),
                message_id.as_deref(),
            )?;
        }
        Ok(())
    }
}

/// The status a caller sees for a row: `queued` for a scheduled `draft`.
pub fn display_status(draft: &Draft) -> &'static str {
    match draft.status {
        DraftStatus::Draft if draft.send_after.is_some() => "queued",
        DraftStatus::Draft => "drafted",
        DraftStatus::PendingReview => "pending_review",
        DraftStatus::Blocked => "blocked",
        DraftStatus::Sending => "sending",
        DraftStatus::Syncing => "syncing",
        DraftStatus::DeliveryUncertain => "delivery_uncertain",
        DraftStatus::Sent => "sent",
        DraftStatus::Discarded => "discarded",
    }
}

fn attempt_field(row: &LeaseRow, key: &str) -> Value {
    row.draft
        .metadata
        .as_ref()
        .and_then(|m| m.get(SEND_ATTEMPT_KEY))
        .and_then(|a| a.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

fn attempt_agent(row: &LeaseRow) -> Option<String> {
    row.attempt()?
        .get("agent_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn push_history(metadata: &mut Map<String, Value>, attempt: Value) {
    let history = metadata
        .entry(SEND_ATTEMPT_HISTORY_KEY.to_string())
        .or_insert_with(|| json!([]));
    if !history.is_array() {
        *history = json!([]);
    }
    if let Value::Array(items) = history {
        items.push(attempt);
        let overflow = items.len().saturating_sub(MAX_ATTEMPT_HISTORY);
        items.drain(0..overflow);
    }
}

fn sent_within_window(draft: &Draft) -> bool {
    draft
        .sent_at
        .as_deref()
        .and_then(parse_sqlite_time)
        .is_some_and(|t| (Utc::now() - t).num_seconds() < IMPLICIT_SENT_WINDOW_SECONDS)
}

/// SQLite `datetime('now')` text (`YYYY-MM-DD HH:MM:SS`, UTC) or RFC 3339.
fn parse_sqlite_time(value: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .map(|t| t.and_utc())
        .ok()
        .or_else(|| {
            DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|t| t.with_timezone(&Utc))
        })
}

/// Carry the stored server-owned keys over a caller's replacement metadata.
pub(crate) fn preserve_server_owned(stored: Option<&Value>, replacement: &mut Value) {
    let Some(obj) = replacement.as_object_mut() else {
        return;
    };
    for key in SERVER_OWNED_METADATA_KEYS {
        obj.remove(*key);
        if let Some(value) = stored.and_then(|m| m.get(*key)) {
            obj.insert((*key).to_string(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drafts::QueueContext;

    fn file_db(dir: &Path) -> Database {
        let db = Database::open(&dir.join("envelope.db")).unwrap();
        db.test_insert_account_row("acc1", "sender@example.test")
            .ok();
        db
    }

    fn memory_db() -> Database {
        let db = Database::open_memory().unwrap();
        db.test_insert_account_row("acc1", "sender@example.test")
            .unwrap();
        db
    }

    fn intent<'a>(key: IntentKey<'a>, text: &'a str) -> NewSendIntent<'a> {
        NewSendIntent {
            account_id: "acc1",
            principal: "local",
            agent_id: None,
            surface: "cli_send",
            key,
            created_by: "cli",
            to: "Alice <Alice@Example.TEST>",
            cc: None,
            bcc: Some("audit@example.test"),
            reply_to: None,
            subject: "Crash test",
            text: Some(text),
            html: None,
            in_reply_to: None,
            attachments: &[],
            metadata: json!({"agent_body_text": text}),
        }
    }

    fn created(db: &Database, key: IntentKey<'_>, text: &str) -> Draft {
        match db.find_or_create_send_intent(&intent(key, text)).unwrap() {
            IntentLookup::Created(d) => d,
            other => panic!("expected a new intent, got {other:?}"),
        }
    }

    fn claim(db: &Database, draft: &Draft) -> AttemptClaim {
        let start = AttemptStart::new("<m1@example.test>", "cli_send", None);
        db.claim_send_attempt(&draft.id, draft.revision, ClaimMode::Immediate, &start)
            .unwrap()
            .expect("claim")
    }

    fn status(db: &Database, id: &str) -> DraftStatus {
        db.get_draft(id).unwrap().unwrap().status
    }

    fn phase(db: &Database, id: &str) -> Option<String> {
        db.get_draft(id)
            .unwrap()
            .unwrap()
            .metadata
            .and_then(|m| m["send_attempt"]["phase"].as_str().map(str::to_string))
    }

    fn receipts(db: &Database, id: &str) -> Vec<(String, Value)> {
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT action_status, action_taken FROM action_log
                 WHERE draft_id = ?1 AND action_type = 'send' ORDER BY rowid",
            )
            .unwrap();
        stmt.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                serde_json::from_str(&r.get::<_, String>(1)?).unwrap(),
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    }

    fn dead(_: &AttemptOwner, _: &str) -> Liveness {
        Liveness::Dead
    }
    fn alive(_: &AttemptOwner, _: &str) -> Liveness {
        Liveness::Alive
    }
    fn unknown(_: &AttemptOwner, _: &str) -> Liveness {
        Liveness::Unknown
    }

    // ── Digests ─────────────────────────────────────────────────────────

    /// Vectors from the Mailroom bench's Python `payload_hash`, so a receipt's
    /// `semantic_sha256` can be compared with what the sink received.
    #[test]
    fn semantic_digest_matches_the_bench_vectors() {
        assert_eq!(
            semantic_sha256(
                "Crash test",
                &["alice@example.test".to_string()],
                "Line one\nLine two"
            ),
            "523f0d8bc7e76d999b5ce167fed8429b8e54365cd26984d1b84fc28f5594881e"
        );
        assert_eq!(
            semantic_sha256(
                "  Café ☕ 😀 \"q\" \\ ",
                &[
                    "Bob@Example.TEST".to_string(),
                    "alice@example.test".to_string(),
                    "alice@example.test".to_string()
                ],
                "Hé\r\nline two   \r\n\r\n\ttabbed\u{1}\n\n"
            ),
            "0d73d64178ac10424d436a455e46ee99c82aa5404d58f9ac0a5c08f0fe61f515"
        );
        assert_eq!(
            semantic_sha256("", &["a@b.c".to_string()], ""),
            "bb7bfd929993b80ebe337e20a47579c519b1e27589483547ae6e063fc1d2cdce"
        );
    }

    #[test]
    fn payload_digest_keeps_local_part_case_and_recipient_roles() {
        let base = intent(IntentKey::Fingerprint, "hi");
        let digest = |f: &dyn Fn(&mut NewSendIntent<'_>)| {
            let mut spec = intent(IntentKey::Fingerprint, "hi");
            f(&mut spec);
            PayloadFields::of_intent(&spec).payload_sha256().unwrap()
        };
        let original = PayloadFields::of_intent(&base).payload_sha256().unwrap();
        assert_eq!(original, digest(&|s| s.to = "Alice <Alice@example.test>"));
        assert_ne!(original, digest(&|s| s.to = "Alice <alice@example.test>"));
        assert_ne!(
            original,
            digest(&|s| {
                s.to = "audit@example.test";
                s.bcc = Some("Alice <Alice@Example.TEST>");
            })
        );
        assert_ne!(original, digest(&|s| s.html = Some("<p>hi</p>")));
        assert_ne!(original, digest(&|s| s.text = Some("")));
    }

    // ── Intents ─────────────────────────────────────────────────────────

    #[test]
    fn an_identical_request_finds_its_intent() {
        let db = memory_db();
        let first = created(&db, IntentKey::Fingerprint, "hi");
        match db
            .find_or_create_send_intent(&intent(IntentKey::Fingerprint, "hi"))
            .unwrap()
        {
            IntentLookup::Existing(d) => assert_eq!(d.id, first.id),
            other => panic!("expected the first intent, got {other:?}"),
        }
        let different = created(&db, IntentKey::Fingerprint, "other body");
        assert_ne!(different.id, first.id);
    }

    #[test]
    fn an_explicit_key_with_another_payload_is_a_conflict() {
        let db = memory_db();
        let first = created(&db, IntentKey::Explicit("op-1"), "hi");
        match db
            .find_or_create_send_intent(&intent(IntentKey::Explicit("op-1"), "changed"))
            .unwrap()
        {
            IntentLookup::KeyConflict(d) => assert_eq!(d.id, first.id),
            other => panic!("expected a conflict, got {other:?}"),
        }
        // The same content under another key is a deliberate second message.
        let second = created(&db, IntentKey::Explicit("op-2"), "hi");
        assert_ne!(second.id, first.id);
    }

    #[test]
    fn an_explicit_key_outlives_discard_and_a_fingerprint_does_not() {
        let db = memory_db();
        let keyed = created(&db, IntentKey::Explicit("op-1"), "hi");
        let plain = created(&db, IntentKey::Fingerprint, "hi");
        assert!(db.discard_draft(&keyed.id).unwrap());
        assert!(db.discard_draft(&plain.id).unwrap());
        assert!(matches!(
            db.find_or_create_send_intent(&intent(IntentKey::Explicit("op-1"), "hi"))
                .unwrap(),
            IntentLookup::Existing(d) if d.id == keyed.id
        ));
        assert!(matches!(
            db.find_or_create_send_intent(&intent(IntentKey::Fingerprint, "hi"))
                .unwrap(),
            IntentLookup::Created(d) if d.id != plain.id
        ));
    }

    #[test]
    fn keys_are_scoped_to_the_principal() {
        let db = memory_db();
        let mine = created(&db, IntentKey::Explicit("op-1"), "hi");
        let mut theirs = intent(IntentKey::Explicit("op-1"), "hi");
        theirs.principal = "agent:other";
        match db.find_or_create_send_intent(&theirs).unwrap() {
            IntentLookup::Created(d) => assert_ne!(d.id, mine.id),
            other => panic!("another principal must not see my key: {other:?}"),
        }
    }

    #[test]
    fn a_fingerprint_matches_an_old_uncertain_intent_but_not_an_old_sent_one() {
        let db = memory_db();
        let uncertain = created(&db, IntentKey::Fingerprint, "one");
        let c = claim(&db, &uncertain);
        assert!(
            db.park_attempt_uncertain(&uncertain.id, &c.token, "test", None)
                .unwrap()
        );
        let sent = created(&db, IntentKey::Fingerprint, "two");
        let c = claim(&db, &sent);
        db.finish_attempt_sent(&sent.id, &c.token, "<m1@example.test>", json!({}))
            .unwrap();
        db.conn()
            .execute(
                "UPDATE drafts SET sent_at = datetime('now', '-16 minutes'),
                    created_at = datetime('now', '-2 days')",
                [],
            )
            .unwrap();
        assert!(matches!(
            db.find_or_create_send_intent(&intent(IntentKey::Fingerprint, "one")).unwrap(),
            IntentLookup::Existing(d) if d.id == uncertain.id
        ));
        assert!(matches!(
            db.find_or_create_send_intent(&intent(IntentKey::Fingerprint, "two"))
                .unwrap(),
            IntentLookup::Created(_)
        ));
    }

    #[test]
    fn an_edited_intent_row_no_longer_answers_for_the_request() {
        let db = memory_db();
        let first = created(&db, IntentKey::Explicit("op-1"), "hi");
        db.update_draft_content(&first.id, None, None, None, None, Some("edited"), None)
            .unwrap();
        assert!(matches!(
            db.find_or_create_send_intent(&intent(IntentKey::Explicit("op-1"), "hi"))
                .unwrap(),
            IntentLookup::KeyConflict(_)
        ));
    }

    /// Two processes racing the identical request end with one row.
    #[test]
    fn racing_identical_requests_record_one_intent() {
        let dir = tempfile::tempdir().unwrap();
        file_db(dir.path());
        let path = dir.path().join("envelope.db");
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let db = Database::open(&path).unwrap();
                    match db
                        .find_or_create_send_intent(&intent(IntentKey::Fingerprint, "race"))
                        .unwrap()
                    {
                        IntentLookup::Created(d) | IntentLookup::Existing(d) => d.id,
                        IntentLookup::KeyConflict(d) => panic!("conflict {}", d.id),
                    }
                })
            })
            .collect();
        let ids: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
        let db = Database::open(&path).unwrap();
        let rows: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn generic_metadata_writes_cannot_forge_or_erase_the_intent() {
        let db = memory_db();
        let draft = created(&db, IntentKey::Explicit("op-1"), "hi");
        let stored = draft.metadata.clone().unwrap()["send_intent"].clone();
        db.set_draft_metadata(
            &draft.id,
            &json!({"send_intent": {"key_sha256": "forged"}, "draft_kind": "new"}),
        )
        .unwrap();
        let after = db.get_draft(&draft.id).unwrap().unwrap().metadata.unwrap();
        assert_eq!(after["send_intent"], stored);
        assert_eq!(after["draft_kind"], "new");
    }

    // ── Attempts and receipts ───────────────────────────────────────────

    #[test]
    fn each_transition_writes_exactly_one_receipt() {
        let db = memory_db();
        let draft = created(&db, IntentKey::Fingerprint, "hi");
        let c = claim(&db, &draft);
        assert!(db.begin_transmitting(&draft.id, &c.token).unwrap());
        db.finish_attempt_sent(
            &draft.id,
            &c.token,
            &c.message_id,
            json!({"kind": "smtp_acceptance", "reply": "250 ok"}),
        )
        .unwrap();

        let got = receipts(&db, &draft.id);
        let summary: Vec<(&str, &str, u64)> = got
            .iter()
            .map(|(s, t)| {
                (
                    s.as_str(),
                    t["phase"].as_str().unwrap(),
                    t["seq"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("sending", "claimed", 1),
                ("sending", "transmitting", 2),
                ("sent", "accepted", 3)
            ]
        );
        let (_, sent) = got.last().unwrap();
        assert_eq!(
            sent["recipients"],
            json!(["alice@example.test", "audit@example.test"])
        );
        assert_eq!(sent["attempt_id"], json!(c.attempt_id));
        assert_eq!(sent["evidence"]["reply"], "250 ok");

        let message_id: Option<String> = db
            .conn()
            .query_row(
                "SELECT message_id FROM action_log WHERE draft_id = ?1 AND action_status = 'sent'",
                params![draft.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(message_id.as_deref(), Some("<m1@example.test>"));
        let event_message_id: Option<String> = db
            .conn()
            .query_row(
                "SELECT message_id FROM events WHERE event_type = 'send_completed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(event_message_id.as_deref(), Some("m1@example.test"));
    }

    #[test]
    fn a_failed_receipt_write_rolls_the_transition_back() {
        let db = memory_db();
        let draft = created(&db, IntentKey::Fingerprint, "hi");
        let c = claim(&db, &draft);
        db.conn()
            .execute_batch(
                "CREATE TRIGGER no_audit BEFORE INSERT ON action_log
                 BEGIN SELECT RAISE(ABORT, 'audit unavailable'); END;",
            )
            .unwrap();
        assert!(db.begin_transmitting(&draft.id, &c.token).is_err());
        assert_eq!(phase(&db, &draft.id).as_deref(), Some("claimed"));
        assert!(
            db.finish_attempt_sent(&draft.id, &c.token, "<m1@example.test>", json!({}))
                .is_err()
        );
        assert_eq!(status(&db, &draft.id), DraftStatus::Sending);
    }

    #[test]
    fn only_a_claimed_attempt_can_be_released_as_not_started() {
        let db = memory_db();
        let draft = created(&db, IntentKey::Fingerprint, "hi");
        let c = claim(&db, &draft);
        assert!(db.begin_transmitting(&draft.id, &c.token).unwrap());
        assert!(
            !db.release_sending_draft(&draft.id, &c.token, DraftStatus::Draft)
                .unwrap()
        );
        assert!(
            !db.release_attempt(
                &draft.id,
                &c.token,
                DraftStatus::Draft,
                ReleaseBasis::NotStarted,
                "test",
                None,
                None
            )
            .unwrap()
        );
        assert_eq!(status(&db, &draft.id), DraftStatus::Sending);
        // An authoritative refusal of the body proves nothing was accepted.
        assert!(
            db.release_attempt(
                &draft.id,
                &c.token,
                DraftStatus::Draft,
                ReleaseBasis::Refused,
                "smtp_refused",
                Some(json!({"reply_code": 451})),
                None
            )
            .unwrap()
        );
        assert_eq!(status(&db, &draft.id), DraftStatus::Draft);
    }

    #[test]
    fn a_released_claim_cannot_start_transmitting() {
        let db = memory_db();
        let draft = created(&db, IntentKey::Fingerprint, "hi");
        let c = claim(&db, &draft);
        assert!(
            db.release_sending_draft(&draft.id, &c.token, DraftStatus::Draft)
                .unwrap()
        );
        assert!(!db.begin_transmitting(&draft.id, &c.token).unwrap());
    }

    #[test]
    fn a_reclaim_starts_a_new_attempt_and_keeps_the_old_one() {
        let db = memory_db();
        let draft = created(&db, IntentKey::Fingerprint, "hi");
        let first = claim(&db, &draft);
        assert!(
            db.release_sending_draft(&draft.id, &first.token, DraftStatus::Draft)
                .unwrap()
        );
        let start = AttemptStart::new("<m2@example.test>", "cli_send", None);
        let second = db
            .claim_send_attempt(&draft.id, draft.revision, ClaimMode::Immediate, &start)
            .unwrap()
            .unwrap();
        assert_ne!(second.attempt_id, first.attempt_id);
        let meta = db.get_draft(&draft.id).unwrap().unwrap().metadata.unwrap();
        assert_eq!(meta["send_attempt"]["message_id"], "<m2@example.test>");
        assert_eq!(
            meta["send_attempt_history"][0]["attempt_id"],
            json!(first.attempt_id)
        );
        assert_eq!(meta["send_attempt_history"][0]["phase"], "released");
    }

    // ── Recovery ────────────────────────────────────────────────────────

    #[test]
    fn a_dead_owner_is_released_before_the_body_and_parked_after_it() {
        let db = memory_db();
        let before = created(&db, IntentKey::Fingerprint, "one");
        claim(&db, &before);
        let after = created(&db, IntentKey::Fingerprint, "two");
        let c = claim(&db, &after);
        assert!(db.begin_transmitting(&after.id, &c.token).unwrap());

        let report = db.reconcile_stale_sending(Utc::now(), &dead).unwrap();
        assert_eq!(report.released, vec![before.id.clone()]);
        assert_eq!(report.parked, vec![after.id.clone()]);
        assert_eq!(status(&db, &before.id), DraftStatus::Draft);
        assert_eq!(status(&db, &after.id), DraftStatus::DeliveryUncertain);
        let (status_label, body) = receipts(&db, &after.id).pop().unwrap();
        assert_eq!(status_label, "delivery_uncertain");
        assert_eq!(body["reason"], "owner_dead");
        assert_eq!(body["evidence"]["phase"], "transmitting");
    }

    #[test]
    fn a_live_owner_is_left_alone_however_old() {
        let db = memory_db();
        let draft = created(&db, IntentKey::Fingerprint, "hi");
        let c = claim(&db, &draft);
        assert!(db.begin_transmitting(&draft.id, &c.token).unwrap());
        let later = Utc::now() + chrono::Duration::hours(6);
        let report = db.reconcile_stale_sending(later, &alive).unwrap();
        assert_eq!(report.left_alive, 1);
        assert_eq!(status(&db, &draft.id), DraftStatus::Sending);
    }

    #[test]
    fn an_unknown_owner_is_resolved_only_after_the_lease() {
        let db = memory_db();
        let claimed = created(&db, IntentKey::Fingerprint, "one");
        claim(&db, &claimed);
        let transmitting = created(&db, IntentKey::Fingerprint, "two");
        let c = claim(&db, &transmitting);
        assert!(db.begin_transmitting(&transmitting.id, &c.token).unwrap());

        let report = db.reconcile_stale_sending(Utc::now(), &unknown).unwrap();
        assert_eq!(report.left_alive, 2);
        let later = Utc::now() + chrono::Duration::seconds(SEND_LEASE_SECONDS + 1);
        let report = db.reconcile_stale_sending(later, &unknown).unwrap();
        assert_eq!(report.released, vec![claimed.id]);
        assert_eq!(report.parked, vec![transmitting.id]);
    }

    #[test]
    fn a_scheduled_row_released_by_recovery_is_due_again() {
        let db = memory_db();
        let draft = created(&db, IntentKey::Fingerprint, "hi");
        db.queue_draft_for_send(
            &draft.id,
            draft.revision,
            "2000-01-01T00:00:00Z",
            &json!({}),
            &QueueContext::STORE,
        )
        .unwrap();
        let start = AttemptStart::new("<m1@example.test>", "sweep", None);
        db.claim_send_attempt(&draft.id, draft.revision, ClaimMode::Due, &start)
            .unwrap()
            .unwrap();
        db.reconcile_stale_sending(Utc::now(), &dead).unwrap();
        let due = db.list_drafts_due_for_send().unwrap();
        assert_eq!(
            due.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            vec![draft.id.as_str()]
        );
    }

    /// A row claimed by a binary that kept no attempt record, or whose stored
    /// attempt belongs to an older lease, has no trustworthy phase: it is
    /// parked (never released) once the lease has passed.
    #[test]
    fn a_claim_without_a_matching_attempt_is_parked_after_the_lease() {
        let db = memory_db();
        let draft = created(&db, IntentKey::Fingerprint, "hi");
        let c = claim(&db, &draft);
        db.conn()
            .execute(
                "UPDATE drafts SET operation_token = 'older-binary' WHERE id = ?1",
                params![draft.id],
            )
            .unwrap();
        let _ = c;
        let now = Utc::now();
        assert_eq!(
            db.reconcile_stale_sending(now, &dead).unwrap().left_alive,
            1
        );
        let later = now + chrono::Duration::seconds(SEND_LEASE_SECONDS + 60);
        assert_eq!(
            db.reconcile_stale_sending(later, &dead).unwrap().parked,
            vec![draft.id.clone()]
        );
        assert_eq!(status(&db, &draft.id), DraftStatus::DeliveryUncertain);
    }

    /// The reconciler decides and writes under one write lock, so a worker
    /// cannot move its attempt to `transmitting` between the decision and the
    /// write. Whoever commits first wins; the loser changes nothing.
    #[test]
    fn recovery_and_the_worker_cannot_both_win_the_claimed_phase() {
        let dir = tempfile::tempdir().unwrap();
        let worker = file_db(dir.path());
        worker
            .conn()
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        let recovery = Database::open(&dir.path().join("envelope.db")).unwrap();
        let draft = created(&worker, IntentKey::Fingerprint, "hi");
        let c = claim(&worker, &draft);

        let attempted = std::cell::Cell::new(None);
        let report = recovery
            .reconcile_stale_sending(Utc::now(), &|_, _| {
                // The worker tries to advance while recovery holds the lock.
                attempted.set(Some(
                    worker.begin_transmitting(&draft.id, &c.token).is_err(),
                ));
                Liveness::Dead
            })
            .unwrap();
        assert_eq!(attempted.get(), Some(true), "the worker must be locked out");
        assert_eq!(report.released, vec![draft.id.clone()]);
        assert!(!worker.begin_transmitting(&draft.id, &c.token).unwrap());

        // The other order: the worker commits first, recovery must park.
        let other = created(&worker, IntentKey::Fingerprint, "second");
        let c2 = claim(&worker, &other);
        assert!(worker.begin_transmitting(&other.id, &c2.token).unwrap());
        let report = recovery.reconcile_stale_sending(Utc::now(), &dead).unwrap();
        assert_eq!(report.parked, vec![other.id.clone()]);
    }

    #[test]
    fn a_late_acceptance_from_the_same_attempt_resolves_the_parked_row() {
        let db = memory_db();
        let draft = created(&db, IntentKey::Fingerprint, "hi");
        let c = claim(&db, &draft);
        assert!(db.begin_transmitting(&draft.id, &c.token).unwrap());
        db.reconcile_stale_sending(Utc::now(), &dead).unwrap();
        assert_eq!(status(&db, &draft.id), DraftStatus::DeliveryUncertain);

        assert!(
            db.finish_attempt_sent(&draft.id, "someone-else", "<m1@example.test>", json!({}))
                .is_err()
        );
        db.finish_attempt_sent(
            &draft.id,
            &c.token,
            "<m1@example.test>",
            json!({"kind": "smtp_acceptance"}),
        )
        .unwrap();
        assert_eq!(status(&db, &draft.id), DraftStatus::Sent);
        let (_, body) = receipts(&db, &draft.id).pop().unwrap();
        assert_eq!(body["reason"], "late_acceptance");
    }

    // ── Owner locks ─────────────────────────────────────────────────────

    #[test]
    fn owner_liveness_follows_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let me = AttemptOwner::current();
        let lock = AttemptLock::acquire(dir.path(), "a1").unwrap();
        assert_eq!(owner_liveness(Some(dir.path()), &me, "a1"), Liveness::Alive);
        drop(lock);
        assert_eq!(owner_liveness(Some(dir.path()), &me, "a1"), Liveness::Dead);

        // A killed process leaves its lock file behind, unlocked.
        std::fs::write(dir.path().join("a2.lock"), b"").unwrap();
        assert_eq!(owner_liveness(Some(dir.path()), &me, "a2"), Liveness::Dead);

        let elsewhere = AttemptOwner {
            pid: 1,
            host: "another-host.invalid".into(),
        };
        assert_eq!(
            owner_liveness(Some(dir.path()), &elsewhere, "a1"),
            Liveness::Unknown
        );
        assert_eq!(owner_liveness(None, &me, "a1"), Liveness::Unknown);
    }

    #[test]
    fn the_lock_directory_sits_next_to_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = file_db(dir.path());
        let canonical = dir.path().canonicalize().unwrap().join("send-locks");
        assert_eq!(
            db.send_lock_dir().map(|p| p
                .parent()
                .unwrap()
                .canonicalize()
                .unwrap()
                .join("send-locks")),
            Some(canonical)
        );
        assert_eq!(memory_db().send_lock_dir(), None);
    }

    #[test]
    fn idempotency_keys_are_bounded_printable_ascii() {
        assert!(validate_idempotency_key("op-1:retry_2").is_ok());
        assert!(validate_idempotency_key("").is_err());
        assert!(validate_idempotency_key("has space").is_err());
        assert!(validate_idempotency_key("é").is_err());
        assert!(validate_idempotency_key(&"k".repeat(MAX_IDEMPOTENCY_KEY_LEN + 1)).is_err());
    }
}
