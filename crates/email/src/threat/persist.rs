// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Where verdicts live and what they cause.
//!
//! - `message_scores` dimension `threat` (so `score_above threat N` rules
//!   match) and the `threat:*` tags;
//! - one pre-acked `threat_verdict` event per scan, payload = the verdict
//!   plus the content fingerprint of the bytes it judged. A stored verdict
//!   and a Mark safe apply only to a message with the same fingerprint;
//! - quarantine: `tag` adds `threat:quarantined`; `move` also runs the shipped,
//!   editable rule `score_above threat 70 → move Envelope/Quarantine` through
//!   the unified executor as agent `envelope:threat`. Only `dangerous` mail
//!   is ever quarantined;
//! - the attachment gate every download/upload chokepoint calls.

use anyhow::{Context, Result, anyhow, bail};
use envelope_email_store::Database;
use envelope_email_store::event_catalog::{LABEL_APPLIED, LOOKUP_PERFORMED, THREAT_VERDICT};
use envelope_email_store::models::{Event, MessageScore, MessageTag, Rule};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Analyzer, FINGERPRINT_KEY_PREFIX, Level, LookupLog, LookupRecord, Quarantine, Signal,
    TAG_DANGEROUS, TAG_FALSE_POSITIVE, TAG_MALWARE, TAG_QUARANTINED, TAG_SUSPICIOUS,
    THREAT_DIMENSION, ThreatConfig, ThreatInput, ThreatVerdict, configured_analyzers, evaluate,
};
use crate::imap::{self, ImapClient};
use crate::rule_exec::{
    self, ActionAttribution, ActionSource, ExecDb, ImapRuleMailbox, MessageTarget, RuleMailbox,
    RuleRunReport, RunAccount,
};
use crate::rules::{Action, ConfirmableAction, MatchExpr, MessageContext, StoredRuleAction};

pub const QUARANTINE_FOLDER: &str = "Envelope/Quarantine";
pub const QUARANTINE_RULE_NAME: &str = "Envelope threat quarantine";

/// Whether `folder` is the quarantine folder: compared trimmed and
/// case-insensitive, with `.` or `/` as the separator, with or without an
/// `INBOX` prefix.
pub fn is_quarantine_folder(folder: &str) -> bool {
    let normalized = folder.trim().to_ascii_lowercase().replace('.', "/");
    let normalized = normalized.trim_matches('/');
    let normalized = normalized.strip_prefix("inbox/").unwrap_or(normalized);
    normalized.eq_ignore_ascii_case(QUARANTINE_FOLDER)
}

/// Whether `name` is the shipped quarantine rule's, compared trimmed and
/// case-insensitive.
pub fn is_quarantine_rule_name(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case(QUARANTINE_RULE_NAME)
}

/// Whether a rule with this definition sets a threat verdict, selects mail
/// by one, or moves mail into quarantine. Under an agent token only the
/// operator writes or enables such a rule.
pub fn rule_touches_threat_state(match_expr: &MatchExpr, action: &Action) -> bool {
    let moves_to_quarantine = match action {
        Action::Move(dest) => is_quarantine_folder(dest),
        Action::Confirm { then, .. } => then.iter().any(
            |step| matches!(step, ConfirmableAction::Move(dest) if is_quarantine_folder(dest)),
        ),
        _ => false,
    };
    moves_to_quarantine || action.sets_threat_tag() || match_expr.references_threat_verdict()
}
/// Agent id every engine-driven action and event is attributed to.
pub const THREAT_AGENT_ID: &str = "envelope:threat";
/// `score_above` is strict and scores are whole numbers, so `> 69.5` is
/// exactly "dangerous" (`>= 70`).
pub const QUARANTINE_THRESHOLD: f64 = 69.5;
/// Stable HTTP/CLI code for a refused attachment.
pub const ATTACHMENT_BLOCKED: &str = "attachment_blocked";
/// Stable code for a Mark safe with no content fingerprint to bind to.
pub const RESCAN_REQUIRED: &str = "rescan_required";
/// How many of a Message-ID's newest events a fingerprint lookup reads.
const FINGERPRINT_SEARCH_LIMIT: usize = 1000;

/// Mailbox sources of raw message bytes for scanning. Implementations must
/// read without setting `\Seen` (EXAMINE + BODY.PEEK[]).
#[allow(async_fn_in_trait)]
pub trait RawFetch {
    async fn fetch_raw(&mut self, folder: &str, uid: u32) -> Result<Option<Vec<u8>>>;
}

impl RawFetch for ImapRuleMailbox<'_> {
    async fn fetch_raw(&mut self, folder: &str, uid: u32) -> Result<Option<Vec<u8>>> {
        imap::fetch_raw_message(self.client, folder, uid)
            .await
            .with_context(|| format!("failed to fetch UID {uid} in {folder} for scanning"))
    }
}

/// Headers the scan read, for the rule context and the event row, plus the
/// outside lookups the scan made.
#[derive(Debug, Clone, Default)]
pub struct ScannedMessage {
    /// [`super::sole_message_id`] of the message.
    pub message_id: Option<String>,
    /// [`super::message_id_values`] of the scanned bytes.
    pub observed_message_ids: Vec<String>,
    pub from_addr: String,
    pub to_addr: String,
    pub subject: String,
    /// Store with [`record_lookups`].
    pub lookups: Vec<LookupRecord>,
    /// [`super::content_fingerprint`] of the scanned bytes.
    pub content_fingerprint: Option<String>,
}

/// Fill the correspondent facts from the local store.
pub fn load_ledger(db: &Database, account_id: &str, input: &mut ThreatInput) {
    input.ledger = db
        .correspondent_facts(account_id, &input.from_addr, input.from_display.as_deref())
        .map_err(|e| format!("correspondent ledger unreadable: {e}"));
}

/// Parse raw bytes, set the account's receiving mail domain, and load the
/// ledger: the part of a scan that needs the database.
pub fn prepare_input(
    db: &Database,
    account_id: &str,
    account_address: &str,
    raw: &[u8],
    config: &ThreatConfig,
) -> Result<ThreatInput, String> {
    let mut input = ThreatInput::from_raw(raw, account_address)?;
    let imap_host = db
        .get_account(account_id)
        .map_err(|e| format!("account {account_id} unreadable: {e}"))?
        .map(|account| account.imap_host);
    input.receiver_domain = config.receiver_domain(account_address, imap_host.as_deref());
    load_ledger(db, account_id, &mut input);
    Ok(input)
}

/// Run the analyzers `config` enables. With clamd or reputation on this does
/// blocking network I/O, so async callers holding the database run it
/// outside the lock (see `scan_one`).
pub fn evaluate_input(
    input: Result<ThreatInput, String>,
    config: &ThreatConfig,
) -> (ThreatVerdict, ScannedMessage) {
    let log = LookupLog::default();
    match configured_analyzers(config, &log) {
        Ok(analyzers) => evaluate_with(input, config, &analyzers, &log),
        Err(reason) => (
            ThreatVerdict::unavailable(reason),
            input.map(|i| scanned_message(&i)).unwrap_or_default(),
        ),
    }
}

/// [`evaluate_input`] with explicit analyzers and the log they write to.
pub fn evaluate_with(
    input: Result<ThreatInput, String>,
    config: &ThreatConfig,
    analyzers: &[Box<dyn Analyzer>],
    log: &LookupLog,
) -> (ThreatVerdict, ScannedMessage) {
    let input = match input {
        Ok(input) => input,
        Err(reason) => {
            return (
                ThreatVerdict::unavailable(reason),
                ScannedMessage::default(),
            );
        }
    };
    let verdict = evaluate(&input, analyzers, config);
    let mut scanned = scanned_message(&input);
    scanned.lookups = std::mem::take(&mut *log.lock().unwrap_or_else(|p| p.into_inner()));
    (verdict, scanned)
}

fn scanned_message(input: &ThreatInput) -> ScannedMessage {
    let header = |name: &str| {
        input
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    ScannedMessage {
        message_id: super::sole_message_id_in(&input.headers),
        observed_message_ids: Vec::new(),
        from_addr: input.from_addr.clone(),
        to_addr: header("to").unwrap_or_default(),
        subject: header("subject").unwrap_or_default(),
        lookups: Vec::new(),
        content_fingerprint: None,
    }
}

/// Scan raw bytes with the configured analyzers. Never fails: an unparseable
/// message is an `unavailable` verdict.
pub fn scan_raw(
    db: &Database,
    account_id: &str,
    account_address: &str,
    raw: &[u8],
    config: &ThreatConfig,
) -> (ThreatVerdict, ScannedMessage) {
    let (verdict, mut scanned) = evaluate_input(
        prepare_input(db, account_id, account_address, raw, config),
        config,
    );
    scanned.content_fingerprint = super::content_fingerprint(raw);
    scanned.observed_message_ids = super::message_id_values(raw);
    (verdict, scanned)
}

/// The key a message's threat tags, score and events are stored under: its
/// [`super::sole_message_id`] while no other content has a verdict under that
/// Message-ID, else [`FINGERPRINT_KEY_PREFIX`] and its content fingerprint.
/// So the first content scanned under a Message-ID keeps it, and a later
/// message that reuses it (a collision) never shares, raises or lowers the
/// first one's tags and score. A verdict stored before fingerprints counts
/// as other content. Without a fingerprint the key is the Message-ID; `None`
/// when neither is known.
pub fn threat_key(
    db: &Database,
    account_id: &str,
    message_id: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<Option<String>> {
    let fingerprint_key = |fp: &str| format!("{FINGERPRINT_KEY_PREFIX}{fp}");
    Ok(match (message_id, fingerprint) {
        (Some(mid), Some(fp)) => {
            let held_by_other = db
                .events_for_message(account_id, THREAT_VERDICT, mid, FINGERPRINT_SEARCH_LIMIT)?
                .into_iter()
                .map(stored_verdict)
                .collect::<Result<Vec<_>>>()?
                .iter()
                .any(|stored| stored.content_fingerprint.as_deref() != Some(fp));
            Some(if held_by_other {
                fingerprint_key(fp)
            } else {
                mid.to_string()
            })
        }
        (Some(mid), None) => Some(mid.to_string()),
        (None, Some(fp)) => Some(fingerprint_key(fp)),
        (None, None) => None,
    })
}

/// Where a verdict is stored.
#[derive(Debug, Clone, Copy)]
pub struct VerdictTarget<'a> {
    pub account_id: &'a str,
    pub folder: &'a str,
    pub uid: u32,
    /// The message's [`super::sole_message_id`]. Scores, tags and events are
    /// keyed by [`threat_key`] of this and the fingerprint.
    pub message_id: Option<&'a str>,
    /// [`super::content_fingerprint`] of the message's bytes, stored beside
    /// the verdict. `None` only where no complete bytes were read.
    pub content_fingerprint: Option<&'a str>,
    /// [`super::message_id_values`] of the message. Stored with a verdict on
    /// a message without one usable Message-ID, so the sweep can tell it is
    /// the message still at that folder/UID.
    pub observed_message_ids: &'a [String],
}

/// The `threat_verdict` event payload: the verdict with the fingerprint of
/// the bytes it judged as a sibling field, which readers that parse only a
/// [`ThreatVerdict`] ignore.
#[derive(Debug, Serialize, Deserialize)]
struct VerdictPayload {
    #[serde(flatten)]
    verdict: ThreatVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_fingerprint: Option<String>,
    /// The message's Message-ID when other content already held it, so this
    /// verdict is keyed by fingerprint: the record of the collision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reused_message_id: Option<String>,
    /// For a message without one usable Message-ID, the values of its
    /// Message-ID fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    observed_message_ids: Vec<String>,
}

/// True when Mark safe applies to the message with these bytes: its threat
/// `key` carries `threat:false_positive` and a Mark safe was recorded there
/// for this fingerprint. A mark on other bytes with the same Message-ID, or
/// one made before fingerprints, does not apply; without a fingerprint
/// nothing is marked safe.
pub fn is_marked_safe(
    db: &Database,
    account_id: &str,
    key: &str,
    fingerprint: Option<&str>,
) -> Result<bool> {
    let Some(fingerprint) = fingerprint else {
        return Ok(false);
    };
    if !has_safe_tag(db, account_id, key)? {
        return Ok(false);
    }
    Ok(safe_marks(db, account_id, key)?
        .iter()
        .any(|(_, mark)| mark.content_fingerprint.as_deref() == Some(fingerprint)))
}

fn has_safe_tag(db: &Database, account_id: &str, message_id: &str) -> Result<bool> {
    Ok(db
        .get_tags(account_id, message_id)?
        .iter()
        .any(|t| t.tag == TAG_FALSE_POSITIVE))
}

/// A Mark safe as its `label_applied` payload recorded it.
#[derive(Deserialize)]
struct SafeMark {
    label: String,
    #[serde(default)]
    content_fingerprint: Option<String>,
}

/// The Mark safe events for a Message-ID, newest first. A payload that does
/// not parse is not counted as a mark.
fn safe_marks(db: &Database, account_id: &str, message_id: &str) -> Result<Vec<(Event, SafeMark)>> {
    Ok(db
        .events_for_message(
            account_id,
            LABEL_APPLIED,
            message_id,
            FINGERPRINT_SEARCH_LIMIT,
        )?
        .into_iter()
        .filter_map(|event| {
            let mark: SafeMark = serde_json::from_str(event.payload.as_deref()?).ok()?;
            (mark.label == TAG_FALSE_POSITIVE).then_some((event, mark))
        })
        .collect())
}

/// A Mark safe made before fingerprints (stored under the Message-ID) on
/// this folder/UID carries over to the bytes there now, under their threat
/// `key`. Legacy marks made anywhere else stay inert.
fn rebind_legacy_mark(
    db: &Database,
    target: &VerdictTarget<'_>,
    message_id: &str,
    key: &str,
    fingerprint: &str,
) -> Result<()> {
    if !has_safe_tag(db, target.account_id, message_id)?
        || is_marked_safe(db, target.account_id, key, Some(fingerprint))?
    {
        return Ok(());
    }
    let legacy_here = safe_marks(db, target.account_id, message_id)?
        .iter()
        .any(|(event, mark)| {
            mark.content_fingerprint.is_none()
                && event.folder == target.folder
                && event.uid == Some(i64::from(target.uid))
        });
    if legacy_here {
        db.add_tag(
            target.account_id,
            key,
            TAG_FALSE_POSITIVE,
            Some(i64::from(target.uid)),
            Some(target.folder),
        )?;
        record_safe_mark(
            db,
            target,
            key,
            fingerprint,
            "legacy_rebind",
            Some(THREAT_AGENT_ID),
        )?;
    }
    Ok(())
}

/// Persist a verdict: score, level tags, and the `threat_verdict` event.
/// A message marked safe keeps score 0 and no level tags, but the verdict is
/// still recorded so `threat show` can say what the engine saw.
pub fn record_verdict(
    db: &Database,
    target: &VerdictTarget<'_>,
    verdict: &ThreatVerdict,
) -> Result<()> {
    let key = threat_key(
        db,
        target.account_id,
        target.message_id,
        target.content_fingerprint,
    )?;
    if let Some(mid) = key.as_deref() {
        let uid = Some(i64::from(target.uid));
        let folder = Some(target.folder);
        let safe = is_marked_safe(db, target.account_id, mid, target.content_fingerprint)?;
        for tag in [TAG_SUSPICIOUS, TAG_DANGEROUS, TAG_MALWARE] {
            db.remove_tag(target.account_id, mid, tag)?;
        }
        match (verdict.level, safe) {
            (Level::Unavailable, _) => {
                // Unknown is not clean: no score for rules to read as low.
                db.remove_score(target.account_id, mid, THREAT_DIMENSION)?;
            }
            (_, true) => {
                db.set_score(target.account_id, mid, THREAT_DIMENSION, 0.0, uid, folder)?;
            }
            (level, false) => {
                db.set_score(
                    target.account_id,
                    mid,
                    THREAT_DIMENSION,
                    f64::from(verdict.score),
                    uid,
                    folder,
                )?;
                match level {
                    Level::Suspicious => {
                        db.add_tag(target.account_id, mid, TAG_SUSPICIOUS, uid, folder)?
                    }
                    Level::Dangerous => {
                        db.add_tag(target.account_id, mid, TAG_DANGEROUS, uid, folder)?
                    }
                    _ => {}
                }
                if verdict.is_malware() {
                    db.add_tag(target.account_id, mid, TAG_MALWARE, uid, folder)?;
                }
            }
        }
    }
    record_verdict_event(db, target, key.as_deref(), verdict)
}

/// The `threat_verdict` event alone, under the target's threat `key`,
/// without touching scores or tags.
fn record_verdict_event(
    db: &Database,
    target: &VerdictTarget<'_>,
    key: Option<&str>,
    verdict: &ThreatVerdict,
) -> Result<()> {
    let reused_message_id = target
        .message_id
        .filter(|_| key.is_some_and(|k| k.starts_with(FINGERPRINT_KEY_PREFIX)));
    let now = chrono::Utc::now().to_rfc3339();
    let event = Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: target.account_id.to_string(),
        event_type: THREAT_VERDICT.to_string(),
        folder: target.folder.to_string(),
        uid: Some(i64::from(target.uid)),
        message_id: key.map(str::to_string),
        from_addr: None,
        subject: None,
        snippet: None,
        payload: Some(
            serde_json::to_string(&VerdictPayload {
                verdict: verdict.clone(),
                content_fingerprint: target.content_fingerprint.map(str::to_string),
                reused_message_id: reused_message_id.map(str::to_string),
                observed_message_ids: match target.message_id {
                    Some(_) => Vec::new(),
                    None => target.observed_message_ids.to_vec(),
                },
            })
            .context("serialize verdict")?,
        ),
        idempotency_key: None,
        secure_pending: false,
        acked_at: Some(now.clone()),
        created_at: now,
    };
    db.insert_event_with_agent(&event, Some(THREAT_AGENT_ID))
        .context("failed to record threat_verdict event")?;
    Ok(())
}

/// One pre-acked `lookup_performed` event per outside lookup, on the message
/// that caused it. The payload is the record: provider, domain, result.
pub fn record_lookups(
    db: &Database,
    target: &VerdictTarget<'_>,
    lookups: &[LookupRecord],
) -> Result<()> {
    if lookups.is_empty() {
        return Ok(());
    }
    let key = threat_key(
        db,
        target.account_id,
        target.message_id,
        target.content_fingerprint,
    )?;
    for lookup in lookups {
        let now = chrono::Utc::now().to_rfc3339();
        let event = Event {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: target.account_id.to_string(),
            event_type: LOOKUP_PERFORMED.to_string(),
            folder: target.folder.to_string(),
            uid: Some(i64::from(target.uid)),
            message_id: key.clone(),
            from_addr: None,
            subject: None,
            snippet: None,
            payload: Some(serde_json::to_string(lookup).context("serialize lookup")?),
            idempotency_key: None,
            secure_pending: false,
            acked_at: Some(now.clone()),
            created_at: now,
        };
        db.insert_event_with_agent(&event, Some(THREAT_AGENT_ID))
            .context("failed to record lookup_performed event")?;
    }
    Ok(())
}

/// A stored verdict with the message identity its event recorded.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredVerdict {
    pub folder: String,
    pub uid: Option<i64>,
    /// The message's Message-ID, when it had one usable Message-ID.
    pub message_id: Option<String>,
    /// What the verdict's message has its threat tags and score under (see
    /// [`threat_key`]). Not part of any JSON output.
    #[serde(skip)]
    pub key: Option<String>,
    /// For a message without one usable Message-ID, the values of its
    /// Message-ID fields. Not part of any JSON output.
    #[serde(skip)]
    pub observed_message_ids: Vec<String>,
    pub recorded_at: String,
    pub verdict: ThreatVerdict,
    /// The fingerprint of the bytes the verdict judged; `None` for a verdict
    /// stored before fingerprints. Not part of any JSON output.
    #[serde(skip)]
    pub content_fingerprint: Option<String>,
}

impl StoredVerdict {
    /// Whether this verdict is for the message a server reports with
    /// `message_id`: its own Message-ID, or, for a message without one
    /// usable Message-ID, one of the values its scan read (an absent or
    /// empty one matching a scan that read none or an empty one).
    pub fn is_for_message_id(&self, message_id: Option<&str>) -> bool {
        let reported = message_id
            .map(envelope_email_store::canonical_message_id)
            .filter(|m| !m.is_empty());
        let observed = &self.observed_message_ids;
        match (self.message_id.as_deref(), reported) {
            (Some(own), reported) => reported == Some(own),
            (None, Some(reported)) => observed.iter().any(|m| m == reported),
            (None, None) => observed.is_empty() || observed.iter().any(String::is_empty),
        }
    }

    /// Whether this verdict is for the message `seen` describes: the same
    /// fingerprint when the bytes are known, else the same Message-ID.
    pub fn is_for(&self, seen: Seen<'_>) -> bool {
        match seen {
            Seen::Bytes(fingerprint) => {
                fingerprint.is_some() && self.content_fingerprint.as_deref() == fingerprint
            }
            Seen::MessageId(message_id) => self.is_for_message_id(message_id),
        }
    }
}

/// The newest verdict recorded for a folder/UID, with its Message-ID.
pub fn stored_verdict_for_uid(
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<Option<StoredVerdict>> {
    db.latest_event_for_uid(account_id, THREAT_VERDICT, folder, uid)?
        .map(stored_verdict)
        .transpose()
}

/// The stored verdict that judged these bytes at folder/UID: the UID's own
/// verdict when its Message-ID matches and its fingerprint is equal, else
/// the newest verdict for the Message-ID with the same fingerprint. A
/// verdict stored without a fingerprint, or on other bytes that share the
/// Message-ID, never applies.
pub fn matching_verdict(
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
    message_id: Option<&str>,
    fingerprint: &str,
) -> Result<Option<StoredVerdict>> {
    if let Some(own) = stored_verdict_for_uid(db, account_id, folder, uid)?
        && own.message_id.as_deref() == message_id
        && own.content_fingerprint.as_deref() == Some(fingerprint)
    {
        return Ok(Some(own));
    }
    let Some(key) = threat_key(db, account_id, message_id, Some(fingerprint))? else {
        return Ok(None);
    };
    for event in
        db.events_for_message(account_id, THREAT_VERDICT, &key, FINGERPRINT_SEARCH_LIMIT)?
    {
        let stored = stored_verdict(event)?;
        if stored.content_fingerprint.as_deref() == Some(fingerprint) {
            return Ok(Some(stored));
        }
    }
    Ok(None)
}

fn stored_verdict(event: Event) -> Result<StoredVerdict> {
    let payload = event
        .payload
        .ok_or_else(|| anyhow!("threat_verdict event {} has no payload", event.id))?;
    let payload: VerdictPayload =
        serde_json::from_str(&payload).context("stored threat_verdict payload is not a verdict")?;
    let message_id = payload.reused_message_id.or_else(|| {
        event
            .message_id
            .clone()
            .filter(|key| !key.starts_with(FINGERPRINT_KEY_PREFIX))
    });
    Ok(StoredVerdict {
        folder: event.folder,
        uid: event.uid,
        message_id,
        key: event.message_id,
        recorded_at: event.created_at,
        verdict: payload.verdict,
        content_fingerprint: payload.content_fingerprint,
        observed_message_ids: payload.observed_message_ids,
    })
}

/// Scan when there is no verdict or it came from an older engine.
pub fn needs_scan(existing: Option<&ThreatVerdict>) -> bool {
    existing.is_none_or(|v| v.engine_version != super::ENGINE_VERSION)
}

/// Why a chokepoint refused attachment bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttachmentBlock {
    pub code: &'static str,
    pub reason: String,
    pub signals: Vec<Signal>,
}

/// The attachment gate, for an attachment of the message with
/// [`super::sole_message_id`] `message_id` and content `fingerprint`. Refuses
/// bytes that look like malware, under their original name or the sanitized
/// name they are written under; or whose message carries `threat:malware`
/// under its threat key, or under its Message-ID when another message's
/// content holds that (strictness may spread between messages sharing a
/// Message-ID, leniency never does); or whose content has a malware verdict
/// on file. Only a Mark safe bound to this fingerprint releases them, so an
/// attachment fetched without one (part by part, from an over-cap message)
/// is never released by Mark safe.
pub fn attachment_block(
    db: &Database,
    account_id: &str,
    message_id: Option<&str>,
    fingerprint: Option<&str>,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<Option<AttachmentBlock>> {
    let key = threat_key(db, account_id, message_id, fingerprint)?;
    if let Some(key) = key.as_deref()
        && is_marked_safe(db, account_id, key, fingerprint)?
    {
        return Ok(None);
    }
    // Bytes reach disk under the sanitized name, so check it as well as the
    // original; either one blocking refuses.
    let mut signals = super::attachments::gate(filename, content_type, bytes);
    let written = crate::ingress::normalize_attachment_filename(filename);
    if written != filename {
        for signal in super::attachments::gate(&written, content_type, bytes) {
            if !signals.iter().any(|s| s.code == signal.code) {
                signals.push(signal);
            }
        }
    }
    let reason = if !signals.is_empty() {
        Some(format!(
            "attachment looks like malware ({})",
            signals
                .iter()
                .map(|s| s.code.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    } else if let Some(key) = key.as_deref() {
        message_malware_reason(db, account_id, key, message_id, fingerprint)?
    } else {
        None
    };
    Ok(reason.map(|reason| AttachmentBlock {
        code: ATTACHMENT_BLOCKED,
        reason,
        signals,
    }))
}

/// Why the message makes every attachment refused, if it does: a malware
/// tag under its threat key or its Message-ID, or a malware verdict on these
/// exact bytes. When the verdict history is longer than the window read, a
/// malware verdict cannot be ruled out and the attachment is refused.
fn message_malware_reason(
    db: &Database,
    account_id: &str,
    key: &str,
    message_id: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<Option<String>> {
    let tagged = |k: &str| -> Result<bool> {
        Ok(db
            .get_tags(account_id, k)?
            .iter()
            .any(|t| t.tag == TAG_MALWARE))
    };
    if tagged(key)? {
        return Ok(Some("the message is tagged threat:malware".to_string()));
    }
    if let Some(mid) = message_id
        && mid != key
        && tagged(mid)?
    {
        return Ok(Some(
            "a message with this Message-ID is tagged threat:malware".to_string(),
        ));
    }
    let Some(fingerprint) = fingerprint else {
        return Ok(None);
    };
    let events =
        db.events_for_message(account_id, THREAT_VERDICT, key, FINGERPRINT_SEARCH_LIMIT)?;
    let window_full = events.len() >= FINGERPRINT_SEARCH_LIMIT;
    for event in events {
        let stored = stored_verdict(event)?;
        if stored.content_fingerprint.as_deref() == Some(fingerprint) && stored.verdict.is_malware()
        {
            return Ok(Some(
                "the message's stored threat verdict is malware".to_string(),
            ));
        }
    }
    Ok(window_full
        .then(|| "the message has more stored verdicts than the attachment gate reads".to_string()))
}

/// Why an agent may not move, copy or delete this message, if the threat
/// engine holds it: its threat key or its Message-ID carries
/// `threat:quarantined` or `threat:malware`, or a malware or dangerous
/// verdict is stored for these bytes. The check reads content, not the
/// folder, because a server can show one message in several folders (Gmail
/// lists a quarantined message in `[Gmail]/All Mail` too). `raw` is `None`
/// over the fetch cap; the header Message-ID then stands in and any verdict
/// under it counts. A message with neither is held, and so is one whose
/// verdict history is longer than the window read. Only a Mark safe bound to
/// these bytes releases it.
pub fn held_reason(
    db: &Database,
    account_id: &str,
    raw: Option<&[u8]>,
    header_message_id: Option<&str>,
) -> Result<Option<String>> {
    let (message_id, fingerprint) = match raw {
        Some(raw) => (super::sole_message_id(raw), super::content_fingerprint(raw)),
        None => (
            header_message_id
                .and_then(super::usable_message_id)
                .map(str::to_string),
            None,
        ),
    };
    let (message_id, fingerprint) = (message_id.as_deref(), fingerprint.as_deref());
    let Some(key) = threat_key(db, account_id, message_id, fingerprint)? else {
        return Ok(Some(
            "the message has no Message-ID or readable content to check".to_string(),
        ));
    };
    if is_marked_safe(db, account_id, &key, fingerprint)? {
        return Ok(None);
    }
    let holding_tag = |k: &str| -> Result<Option<String>> {
        Ok(db
            .get_tags(account_id, k)?
            .into_iter()
            .map(|t| t.tag)
            .find(|t| t == TAG_QUARANTINED || t == TAG_MALWARE))
    };
    if let Some(tag) = holding_tag(&key)? {
        return Ok(Some(format!("the message is tagged {tag}")));
    }
    if let Some(mid) = message_id
        && mid != key
        && let Some(tag) = holding_tag(mid)?
    {
        return Ok(Some(format!(
            "a message with this Message-ID is tagged {tag}"
        )));
    }
    let events =
        db.events_for_message(account_id, THREAT_VERDICT, &key, FINGERPRINT_SEARCH_LIMIT)?;
    let window_full = events.len() >= FINGERPRINT_SEARCH_LIMIT;
    for event in events {
        let stored = stored_verdict(event)?;
        let same_content =
            fingerprint.is_none_or(|fp| stored.content_fingerprint.as_deref() == Some(fp));
        if !same_content {
            continue;
        }
        if stored.verdict.is_malware() {
            return Ok(Some(
                "the message's stored threat verdict is malware".to_string(),
            ));
        }
        if stored.verdict.level == Level::Dangerous {
            return Ok(Some(
                "the message's stored threat verdict is dangerous".to_string(),
            ));
        }
    }
    Ok(
        window_full
            .then(|| "the message has more stored verdicts than the check reads".to_string()),
    )
}

/// [`held_reason`] for the message at `folder`/`uid`, read from the server
/// without marking it seen. A message that is not there is an error.
pub async fn held_at(
    client: &mut ImapClient,
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<Option<String>> {
    let fetched = imap::fetch_message_with_raw(client, folder, uid)
        .await
        .with_context(|| format!("failed to read UID {uid} in {folder} for the threat check"))?;
    held_of_fetch(db, account_id, folder, uid, fetched)
}

/// [`held_at`] for what [`imap::fetch_message_with_raw`] read at
/// `folder`/`uid`.
pub(crate) fn held_of_fetch(
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
    fetched: Option<(envelope_email_store::models::Message, Option<Vec<u8>>)>,
) -> Result<Option<String>> {
    let (message, raw) =
        fetched.ok_or_else(|| anyhow!("message UID {uid} not found in {folder}"))?;
    held_reason(
        db,
        account_id,
        raw.as_deref(),
        message.message_id.as_deref(),
    )
}

/// Every attachment of `raw` the download gate would refuse, by filename, in
/// message order. The reader uses it to show a blocked attachment as blocked
/// instead of offering a download the server will refuse. Names and content
/// types are read the way the download path reads them.
pub fn blocked_attachments(
    db: &Database,
    account_id: &str,
    raw: &[u8],
) -> Result<Vec<(String, AttachmentBlock)>> {
    use mail_parser::MimeHeaders;
    let parsed = mail_parser::MessageParser::default()
        .parse(raw)
        .ok_or_else(|| anyhow!("message could not be parsed for the attachment gate"))?;
    let message_id = super::sole_message_id(raw);
    let fingerprint = super::content_fingerprint(raw);
    let mut out = Vec::new();
    for attachment in parsed.attachments() {
        let filename = attachment.attachment_name().unwrap_or("unnamed");
        let content_type = attachment
            .content_type()
            .map(|ct| format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("octet-stream")))
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let content_type = crate::ingress::normalize_content_type(&content_type);
        if let Some(block) = attachment_block(
            db,
            account_id,
            message_id.as_deref(),
            fingerprint.as_deref(),
            filename,
            &content_type,
            attachment.contents(),
        )? {
            out.push((filename.to_string(), block));
        }
    }
    Ok(out)
}

/// `threat mark-safe`: tag `threat:false_positive`, clear the level,
/// malware and quarantine tags, zero the score, and log `label_applied` with
/// the content fingerprint the mark is bound to. Refused with
/// [`RESCAN_REQUIRED`] when the target has no fingerprint.
pub fn mark_safe(
    db: &Database,
    target: &VerdictTarget<'_>,
    source: &str,
    agent_id: Option<&str>,
) -> Result<()> {
    let fingerprint = target.content_fingerprint.ok_or_else(|| {
        anyhow!(
            "{RESCAN_REQUIRED}: UID {} in {} has no content fingerprint to bind Mark safe to; \
             open or scan the message first",
            target.uid,
            target.folder
        )
    })?;
    let key = threat_key(db, target.account_id, target.message_id, Some(fingerprint))?
        .ok_or_else(|| anyhow!("UID {} in {} has no threat key", target.uid, target.folder))?;
    let mid = key.as_str();
    let uid = Some(i64::from(target.uid));
    for tag in [TAG_SUSPICIOUS, TAG_DANGEROUS, TAG_MALWARE, TAG_QUARANTINED] {
        db.remove_tag(target.account_id, mid, tag)?;
    }
    db.add_tag(
        target.account_id,
        mid,
        TAG_FALSE_POSITIVE,
        uid,
        Some(target.folder),
    )?;
    db.set_score(
        target.account_id,
        mid,
        THREAT_DIMENSION,
        0.0,
        uid,
        Some(target.folder),
    )?;
    record_safe_mark(db, target, mid, fingerprint, source, agent_id)
}

/// The `label_applied` event, under a threat `key`, that binds a Mark safe
/// to a fingerprint.
fn record_safe_mark(
    db: &Database,
    target: &VerdictTarget<'_>,
    key: &str,
    fingerprint: &str,
    source: &str,
    agent_id: Option<&str>,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let event = Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: target.account_id.to_string(),
        event_type: LABEL_APPLIED.to_string(),
        folder: target.folder.to_string(),
        uid: Some(i64::from(target.uid)),
        message_id: Some(key.to_string()),
        from_addr: None,
        subject: None,
        snippet: None,
        payload: Some(
            json!({
                "label": TAG_FALSE_POSITIVE,
                "source": source,
                "content_fingerprint": fingerprint,
            })
            .to_string(),
        ),
        idempotency_key: None,
        secure_pending: false,
        acked_at: Some(now.clone()),
        created_at: now,
    };
    db.insert_event_with_agent(&event, agent_id)
        .context("failed to record label_applied event")?;
    Ok(())
}

/// The shipped quarantine rule's `(match_expr, action)` JSON.
pub fn quarantine_rule_json() -> (String, String) {
    let match_expr = MatchExpr::ScoreAbove {
        dimension: THREAT_DIMENSION.to_string(),
        threshold: QUARANTINE_THRESHOLD,
    };
    let action = StoredRuleAction {
        action: Action::Move(QUARANTINE_FOLDER.to_string()),
        acknowledged_batch_actions: false,
    };
    (
        serde_json::to_string(&match_expr).expect("static match expr serializes"),
        action.to_json().expect("static action serializes"),
    )
}

/// Install the quarantine rule for an account unless one with its name
/// already exists (a user's edits are kept).
pub fn ensure_quarantine_rule(db: &Database, account_id: &str) -> Result<Rule> {
    if let Some(rule) = db.find_rule_by_name(account_id, QUARANTINE_RULE_NAME)? {
        return Ok(rule);
    }
    let (match_expr, action) = quarantine_rule_json();
    Ok(db.create_rule(
        account_id,
        QUARANTINE_RULE_NAME,
        &match_expr,
        &action,
        0,
        true,
    )?)
}

/// Build the rule context for the scanned message at folder/UID from the
/// stores, its threat data from its own verdict.
fn rule_context(
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
    scanned: &ScannedMessage,
) -> Result<MessageContext> {
    let (tags, scores) = match scanned.message_id.as_deref() {
        Some(mid) => (
            db.get_tags(account_id, mid)?
                .into_iter()
                .map(|t| t.tag)
                .collect(),
            db.get_scores(account_id, mid)?
                .into_iter()
                .map(|s| (s.dimension, s.value))
                .collect(),
        ),
        None => (Vec::new(), Default::default()),
    };
    let mut ctx = MessageContext {
        from_addr: scanned.from_addr.clone(),
        to_addr: scanned.to_addr.clone(),
        subject: scanned.subject.clone(),
        tags,
        scores,
        contact_tags: db.get_contact_tags(account_id, &scanned.from_addr)?,
    };
    bind_threat_context(
        db,
        account_id,
        folder,
        uid,
        Seen::Bytes(scanned.content_fingerprint.as_deref()),
        &mut ctx,
    )?;
    Ok(ctx)
}

/// Replace a rule context's threat data (the `threat` score and every
/// `threat:*` tag) with [`bound_threat`] for this folder/UID.
pub fn bind_threat_context(
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
    seen: Seen<'_>,
    ctx: &mut MessageContext,
) -> Result<()> {
    ctx.tags.retain(|t| !t.starts_with("threat:"));
    ctx.scores.remove(THREAT_DIMENSION);
    let bound = bound_threat(db, account_id, folder, uid, seen)?;
    if let Some(score) = bound.score {
        ctx.scores.insert(THREAT_DIMENSION.to_string(), score.value);
    }
    ctx.tags.extend(bound.tags.into_iter().map(|t| t.tag));
    Ok(())
}

/// What a caller knows of the message at a folder/UID now, to tell whether
/// the verdict stored there is for it.
#[derive(Debug, Clone, Copy)]
pub enum Seen<'a> {
    /// Its bytes: their content fingerprint, `None` when they have none.
    Bytes(Option<&'a str>),
    /// Only its Message-ID, as the server reports it.
    MessageId(Option<&'a str>),
}

/// What a reader fetched of the message it opened at a folder/UID.
#[derive(Debug, Clone, Copy)]
pub enum Opened<'a> {
    /// Its complete bytes.
    Whole(&'a [u8]),
    /// Its parts, read one by one because it is over the whole-message fetch
    /// cap: no complete bytes, only its Message-ID.
    Parts { message_id: Option<&'a str> },
}

impl<'a> Opened<'a> {
    /// A fetch that returned the message's Message-ID and, unless it is over
    /// the cap, its bytes.
    pub fn new(raw: Option<&'a [u8]>, message_id: Option<&'a str>) -> Self {
        match raw {
            Some(raw) => Opened::Whole(raw),
            None => Opened::Parts { message_id },
        }
    }
}

/// The verdict stored at folder/UID when it is for the message opened there
/// ([`StoredVerdict::is_for`]: the same fingerprint when its bytes are at
/// hand, else the same Message-ID). One left by another message at a reused
/// UID is not shown.
pub fn stored_verdict_for(
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
    opened: Opened<'_>,
) -> Result<Option<StoredVerdict>> {
    let fingerprint = match opened {
        Opened::Whole(raw) => super::content_fingerprint(raw),
        Opened::Parts { .. } => None,
    };
    let seen = match opened {
        Opened::Whole(_) => Seen::Bytes(fingerprint.as_deref()),
        Opened::Parts { message_id } => Seen::MessageId(message_id),
    };
    Ok(stored_verdict_for_uid(db, account_id, folder, uid)?.filter(|s| s.is_for(seen)))
}

/// One message's own threat data.
#[derive(Debug, Default)]
pub struct BoundThreat {
    pub score: Option<MessageScore>,
    pub tags: Vec<MessageTag>,
}

/// The `threat` score and `threat:*` tags the verdict stored for this
/// folder/UID gives its message, read under that message's own threat key.
/// Stores keyed by Message-ID are shared by every message with that
/// Message-ID, so they never decide one message's threat data. A UID
/// without a verdict has none, and so does one whose verdict is for another
/// message by `seen` (the UID was reused). A tag or score stored under the
/// key keeps its record; one the verdict implies without a record is dated
/// by it.
pub fn bound_threat(
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
    seen: Seen<'_>,
) -> Result<BoundThreat> {
    let mut bound = BoundThreat::default();
    let Some(stored) =
        stored_verdict_for_uid(db, account_id, folder, uid)?.filter(|stored| stored.is_for(seen))
    else {
        return Ok(bound);
    };
    let verdict = &stored.verdict;
    let key = stored.key.as_deref();
    let safe = match key {
        Some(key) => is_marked_safe(db, account_id, key, stored.content_fingerprint.as_deref())?,
        None => false,
    };
    if verdict.level != Level::Unavailable {
        let value = if safe { 0.0 } else { f64::from(verdict.score) };
        let recorded = match key {
            Some(key) => db
                .get_scores(account_id, key)?
                .into_iter()
                .find(|s| s.dimension == THREAT_DIMENSION),
            None => None,
        };
        bound.score = Some(match recorded {
            Some(score) => MessageScore { value, ..score },
            None => MessageScore {
                account_id: account_id.to_string(),
                message_id: key.unwrap_or_default().to_string(),
                dimension: THREAT_DIMENSION.to_string(),
                value,
                uid: Some(i64::from(uid)),
                folder: Some(folder.to_string()),
                created_at: stored.recorded_at.clone(),
                updated_at: stored.recorded_at.clone(),
            },
        });
    }
    let Some(key) = key else {
        return Ok(bound);
    };
    let recorded = db.get_tags(account_id, key)?;
    let mut names = Vec::new();
    if safe {
        names.push(TAG_FALSE_POSITIVE);
    } else {
        match verdict.level {
            Level::Suspicious => names.push(TAG_SUSPICIOUS),
            Level::Dangerous => names.push(TAG_DANGEROUS),
            Level::Clean | Level::Unavailable => {}
        }
        if verdict.is_malware() {
            names.push(TAG_MALWARE);
        }
        if recorded.iter().any(|t| t.tag == TAG_QUARANTINED) {
            names.push(TAG_QUARANTINED);
        }
    }
    bound.tags = names
        .into_iter()
        .map(|name| {
            recorded
                .iter()
                .find(|t| t.tag == name)
                .cloned()
                .unwrap_or_else(|| MessageTag {
                    account_id: account_id.to_string(),
                    message_id: key.to_string(),
                    tag: name.to_string(),
                    uid: Some(i64::from(uid)),
                    folder: Some(folder.to_string()),
                    created_at: stored.recorded_at.clone(),
                })
        })
        .collect();
    Ok(bound)
}

/// The tags and scores a tag view shows for the message at this folder/UID,
/// fetched as `raw` (`None` when read part by part): those stored under its
/// Message-ID, with the threat data replaced by [`bound_threat`], as rule
/// contexts read it.
pub fn shown_tags_and_scores(
    db: &Database,
    account_id: &str,
    folder: &str,
    uid: u32,
    message_id: &str,
    raw: Option<&[u8]>,
) -> Result<(Vec<MessageTag>, Vec<MessageScore>)> {
    let fingerprint = raw.map(super::content_fingerprint);
    let seen = match &fingerprint {
        Some(fingerprint) => Seen::Bytes(fingerprint.as_deref()),
        None => Seen::MessageId(Some(message_id)),
    };
    let bound = bound_threat(db, account_id, folder, uid, seen)?;
    let mut tags = db.get_tags(account_id, message_id)?;
    tags.retain(|t| !t.tag.starts_with("threat:"));
    tags.extend(bound.tags);
    let mut scores = db.get_scores(account_id, message_id)?;
    scores.retain(|s| s.dimension != THREAT_DIMENSION);
    scores.extend(bound.score);
    Ok((tags, scores))
}

/// What quarantine did to one message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineOutcome {
    NotApplied,
    Tagged,
    Moved,
    /// The rule exists but is disabled, or an edit made it not match.
    RuleDidNotMove,
}

/// Apply `threat.quarantine` to a freshly recorded verdict.
#[allow(clippy::too_many_arguments)]
pub async fn apply_quarantine<M: RuleMailbox, D: ExecDb>(
    mbox: &mut M,
    db: &D,
    account: &RunAccount<'_>,
    folder: &str,
    uid: u32,
    scanned: &ScannedMessage,
    verdict: &ThreatVerdict,
    config: &ThreatConfig,
) -> Result<QuarantineOutcome> {
    if verdict.level != Level::Dangerous || config.quarantine == Quarantine::None {
        return Ok(QuarantineOutcome::NotApplied);
    }
    let fingerprint = scanned.content_fingerprint.as_deref();
    let message_id = scanned.message_id.as_deref();
    let (key, safe) = db
        .with_db(|d| -> Result<(Option<String>, bool)> {
            let key = threat_key(d, account.id, message_id, fingerprint)?;
            let safe = match key.as_deref() {
                Some(key) => is_marked_safe(d, account.id, key, fingerprint)?,
                None => false,
            };
            Ok((key, safe))
        })
        .await?;
    // Keyed by content, so the move's replay guard is per message and
    // never treats another message with this Message-ID as already moved.
    let Some(key) = key else {
        return Ok(QuarantineOutcome::NotApplied);
    };
    let mid = key.as_str();
    if safe {
        return Ok(QuarantineOutcome::NotApplied);
    }
    db.with_db(|d| {
        d.add_tag(
            account.id,
            mid,
            TAG_QUARANTINED,
            Some(i64::from(uid)),
            Some(folder),
        )
    })
    .await
    .context("failed to tag threat:quarantined")?;
    if config.quarantine == Quarantine::Tag {
        return Ok(QuarantineOutcome::Tagged);
    }

    let (rule, ctx) = db
        .with_db(|d| -> Result<(Rule, MessageContext)> {
            Ok((
                ensure_quarantine_rule(d, account.id)?,
                rule_context(d, account.id, folder, uid, scanned)?,
            ))
        })
        .await?;
    if !rule.enabled {
        return Ok(QuarantineOutcome::RuleDidNotMove);
    }
    let (loaded, skipped) = rule_exec::load_rules(vec![rule]);
    if let Some(skip) = skipped.first() {
        bail!(
            "quarantine rule '{}' cannot run: {}",
            skip.rule_name,
            skip.reason
        );
    }
    if let Err(e) = mbox.ensure_folder(QUARANTINE_FOLDER).await {
        tracing::debug!("{QUARANTINE_FOLDER} not created (may already exist): {e:#}");
    }
    let target = MessageTarget {
        account_id: account.id,
        account_email: account.email,
        folder,
        uid,
        message_id: Some(mid),
        ctx: &ctx,
    };
    let attribution = ActionAttribution::new(ActionSource::Rule).with_agent(Some(THREAT_AGENT_ID));
    let mut report = RuleRunReport::default();
    rule_exec::run_rules_on_message(mbox, db, &target, &loaded, &attribution, &mut report).await;
    if let Some(err) = report.log.iter().find(|e| e["status"] == "error") {
        bail!("quarantine move failed: {}", err["error"]);
    }
    let moved = report
        .log
        .iter()
        .any(|e| e["status"] == "ok" || e["status"] == "already_applied");
    Ok(if moved {
        QuarantineOutcome::Moved
    } else {
        QuarantineOutcome::RuleDidNotMove
    })
}

/// One message through the new-mail pass.
#[derive(Debug, Clone, Serialize)]
pub struct PassEntry {
    pub uid: u32,
    pub message_id: Option<String>,
    pub score: u32,
    pub level: Level,
    pub quarantine: QuarantineOutcome,
}

/// The threat half of the new-mail pass: fetch (read-only), scan, record,
/// quarantine. Per-UID failures are returned, never swallowed.
pub async fn scan_new_mail<M: RuleMailbox + RawFetch, D: ExecDb>(
    mbox: &mut M,
    db: &D,
    account: &RunAccount<'_>,
    folder: &str,
    uids: &[u32],
    config: &ThreatConfig,
) -> Vec<(u32, Result<PassEntry>)> {
    let mut out = Vec::with_capacity(uids.len());
    for &uid in uids {
        out.push((uid, scan_one(mbox, db, account, folder, uid, config).await));
    }
    out
}

async fn scan_one<M: RuleMailbox + RawFetch, D: ExecDb>(
    mbox: &mut M,
    db: &D,
    account: &RunAccount<'_>,
    folder: &str,
    uid: u32,
    config: &ThreatConfig,
) -> Result<PassEntry> {
    let raw = mbox
        .fetch_raw(folder, uid)
        .await?
        .ok_or_else(|| anyhow!("UID {uid} vanished from {folder} before it was scanned"))?;
    let input = db
        .with_db(|d| prepare_input(d, account.id, account.email, &raw, config))
        .await;
    // Opt-in analyzers block on clamd and DNS: run them off the async
    // workers and without holding the database.
    let owned = config.clone();
    let (verdict, mut scanned) = tokio::task::spawn_blocking(move || evaluate_input(input, &owned))
        .await
        .context("threat analyzers panicked")?;
    scanned.content_fingerprint = super::content_fingerprint(&raw);
    scanned.observed_message_ids = super::message_id_values(&raw);
    db.with_db(|d| -> Result<()> {
        let target = VerdictTarget {
            account_id: account.id,
            folder,
            uid,
            message_id: scanned.message_id.as_deref(),
            content_fingerprint: scanned.content_fingerprint.as_deref(),
            observed_message_ids: &scanned.observed_message_ids,
        };
        record_verdict(d, &target, &verdict)?;
        record_lookups(d, &target, &scanned.lookups)
    })
    .await?;
    let quarantine =
        apply_quarantine(mbox, db, account, folder, uid, &scanned, &verdict, config).await?;
    Ok(PassEntry {
        uid,
        message_id: scanned.message_id,
        score: verdict.score,
        level: verdict.level,
        quarantine,
    })
}

/// Scan one message on open when no current verdict judged these bytes (and
/// `threat.on_read` is on). Returns the verdict to show.
///
/// A verdict judged the bytes when [`matching_verdict`] finds it with this
/// fingerprint from the current engine. One stored before fingerprints, or
/// from an older engine, is rescanned. One reused from another folder/UID is
/// also recorded under this UID (event only; scores and tags stay as they
/// are).
///
/// A message read part by part ([`Opened::Parts`]) has nothing complete to
/// scan, so this returns the verdict stored for its Message-ID at the UID,
/// if any, and records nothing.
pub fn verdict_on_open(
    db: &Database,
    account_id: &str,
    account_address: &str,
    folder: &str,
    uid: u32,
    opened: Opened<'_>,
    config: &ThreatConfig,
) -> Result<Option<ThreatVerdict>> {
    let raw = match opened {
        Opened::Whole(raw) => raw,
        Opened::Parts { .. } => {
            return Ok(stored_verdict_for(db, account_id, folder, uid, opened)?.map(|s| s.verdict));
        }
    };
    let message_id = super::sole_message_id(raw);
    let fingerprint = super::content_fingerprint(raw);
    let observed = super::message_id_values(raw);
    let here = VerdictTarget {
        account_id,
        folder,
        uid,
        message_id: message_id.as_deref(),
        content_fingerprint: fingerprint.as_deref(),
        observed_message_ids: &observed,
    };
    let key = threat_key(db, account_id, here.message_id, here.content_fingerprint)?;
    if let (Some(mid), Some(key), Some(fingerprint)) =
        (here.message_id, key.as_deref(), here.content_fingerprint)
    {
        rebind_legacy_mark(db, &here, mid, key, fingerprint)?;
    }
    // Bytes without a fingerprint match no stored verdict.
    let matched = match here.content_fingerprint {
        Some(fingerprint) => matching_verdict(
            db,
            account_id,
            folder,
            uid,
            message_id.as_deref(),
            fingerprint,
        )?,
        None => None,
    };
    if !config.enabled || !config.on_read {
        // Nothing scans. A verdict stored here without a fingerprint may be
        // shown; it is never reused or marked safe.
        let shown = match matched {
            Some(m) => Some(m.verdict),
            None => stored_verdict_for_uid(db, account_id, folder, uid)?
                .filter(|own| own.content_fingerprint.is_none() && own.message_id == message_id)
                .map(|own| own.verdict),
        };
        return Ok(shown);
    }
    if let Some(m) = matched
        && !needs_scan(Some(&m.verdict))
    {
        if (m.folder.as_str(), m.uid) != (folder, Some(i64::from(uid))) {
            record_verdict_event(db, &here, key.as_deref(), &m.verdict)?;
        }
        return Ok(Some(m.verdict));
    }
    let (verdict, scanned) = scan_raw(db, account_id, account_address, raw, config);
    let target = VerdictTarget {
        account_id,
        folder,
        uid,
        message_id: scanned.message_id.as_deref(),
        content_fingerprint: scanned.content_fingerprint.as_deref(),
        observed_message_ids: &scanned.observed_message_ids,
    };
    record_verdict(db, &target, &verdict)?;
    record_lookups(db, &target, &scanned.lookups)?;
    Ok(Some(verdict))
}

/// Non-clean and unavailable rates over each message's latest verdict.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ThreatStats {
    pub scanned: u64,
    pub clean: u64,
    pub suspicious: u64,
    pub dangerous: u64,
    pub unavailable: u64,
    pub malware: u64,
    pub non_clean_rate: f64,
    pub unavailable_rate: f64,
}

pub fn stats(db: &Database, account_id: &str) -> Result<ThreatStats> {
    let mut s = ThreatStats::default();
    for payload in db.latest_event_payloads_per_message(account_id, THREAT_VERDICT)? {
        let verdict: ThreatVerdict = serde_json::from_str(&payload)
            .context("stored threat_verdict payload is not a verdict")?;
        s.scanned += 1;
        match verdict.level {
            Level::Clean => s.clean += 1,
            Level::Suspicious => s.suspicious += 1,
            Level::Dangerous => s.dangerous += 1,
            Level::Unavailable => s.unavailable += 1,
        }
        if verdict.is_malware() {
            s.malware += 1;
        }
    }
    if s.scanned > 0 {
        let n = s.scanned as f64;
        s.non_clean_rate = (s.suspicious + s.dangerous) as f64 / n;
        s.unavailable_rate = s.unavailable as f64 / n;
    }
    Ok(s)
}

/// Scan one UID over a plain IMAP client (CLI `threat scan`/`show`).
pub async fn scan_uid(
    client: &mut ImapClient,
    db: &Database,
    account: &RunAccount<'_>,
    folder: &str,
    uid: u32,
    config: &ThreatConfig,
) -> Result<PassEntry> {
    let mut mbox = ImapRuleMailbox {
        client,
        db,
        account_id: account.id,
        agent_run: false,
    };
    scan_one(&mut mbox, db, account, folder, uid, config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::threat::ENGINE_VERSION;

    /// Every fixture here has a fingerprint.
    fn content_fingerprint(raw: &[u8]) -> String {
        crate::threat::content_fingerprint(raw).expect("fixture has a content fingerprint")
    }

    const ACCT: &str = "acct-1";
    const EMAIL: &str = "me@example.org";

    /// Records mailbox calls; serves fixture bytes; never touches a network.
    #[derive(Default)]
    struct FakeMailbox {
        calls: Vec<String>,
        raw: std::collections::HashMap<u32, Vec<u8>>,
    }

    impl RuleMailbox for FakeMailbox {
        async fn resolve_folder(&mut self, dest: &str) -> Result<String> {
            Ok(dest.to_string())
        }
        async fn move_message(&mut self, folder: &str, uid: u32, dest: &str) -> Result<()> {
            self.calls.push(format!("move {folder}/{uid} -> {dest}"));
            Ok(())
        }
        async fn set_flag(&mut self, _: &str, _: u32, _: &str) -> Result<()> {
            unreachable!("threat pass never sets flags")
        }
        async fn remove_flag(&mut self, _: &str, _: u32, _: &str) -> Result<()> {
            unreachable!("threat pass never removes flags")
        }
        async fn delete_message(&mut self, _: &str, _: u32) -> Result<()> {
            unreachable!("threat pass never deletes")
        }
        async fn ensure_folder(&mut self, name: &str) -> Result<()> {
            self.calls.push(format!("create {name}"));
            Ok(())
        }
        async fn list_unsubscribe_headers(
            &mut self,
            _: &str,
            _: u32,
        ) -> Result<(Option<String>, Option<String>)> {
            Ok((None, None))
        }
    }

    impl RawFetch for FakeMailbox {
        async fn fetch_raw(&mut self, _folder: &str, uid: u32) -> Result<Option<Vec<u8>>> {
            Ok(self.raw.get(&uid).cloned())
        }
    }

    fn account() -> RunAccount<'static> {
        RunAccount {
            id: ACCT,
            email: EMAIL,
        }
    }

    /// A lookalike-of-own-domain sender with a disguised executable: dangerous
    /// and malware.
    fn phish(mid: &str) -> Vec<u8> {
        format!(
            "Authentication-Results: mx1.example.org; spf=fail; dmarc=fail\r\n\
             Received: from x by mx1.example.org with ESMTP; Mon, 21 Sep 2026 10:00:00 +0000\r\n\
             Message-ID: <{mid}>\r\n\
             From: IT Desk <it@examp1e.org>\r\n\
             To: me@example.org\r\n\
             Subject: Password expiry\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=b\r\n\r\n\
             --b\r\nContent-Type: text/plain\r\n\r\nverify your account\r\n\
             --b\r\nContent-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=\"invoice.pdf.exe\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\nTVqQAAMAAAAEAAAA\r\n--b--\r\n"
        )
        .into_bytes()
    }

    fn ordinary(mid: &str) -> Vec<u8> {
        format!(
            "Authentication-Results: mx1.example.org; spf=pass; dkim=pass; dmarc=pass\r\n\
             Received: from x by mx1.example.org with ESMTP; Mon, 21 Sep 2026 10:00:00 +0000\r\n\
             Message-ID: <{mid}>\r\nFrom: Alice <alice@partner.example>\r\nTo: me@example.org\r\n\
             Subject: Lunch\r\n\r\nThursday?\r\n"
        )
        .into_bytes()
    }

    fn tags(db: &Database, mid: &str) -> Vec<String> {
        let mut t: Vec<String> = db
            .get_tags(ACCT, mid)
            .unwrap()
            .into_iter()
            .map(|t| t.tag)
            .collect();
        t.sort();
        t
    }

    #[test]
    fn dangerous_verdict_sets_score_tags_and_one_event() {
        let db = Database::open_memory().unwrap();
        let (verdict, scanned) =
            scan_raw(&db, ACCT, EMAIL, &phish("p1@x"), &ThreatConfig::default());
        assert_eq!(verdict.level, Level::Dangerous);
        assert!(verdict.is_malware());
        assert!(
            super::super::explain(&verdict)
                .iter()
                .any(|l| l.contains("sender domain examp1e.org imitates example.org")),
            "{:?}",
            super::super::explain(&verdict)
        );
        record_verdict(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 7,
                message_id: scanned.message_id.as_deref(),
                content_fingerprint: None,
                observed_message_ids: &[],
            },
            &verdict,
        )
        .unwrap();
        assert_eq!(tags(&db, "p1@x"), vec![TAG_DANGEROUS, TAG_MALWARE]);
        let score = db.get_scores(ACCT, "p1@x").unwrap();
        assert_eq!(score[0].dimension, THREAT_DIMENSION);
        assert_eq!(score[0].value, f64::from(verdict.score));

        let stored = stored_verdict_for_uid(&db, ACCT, "INBOX", 7)
            .unwrap()
            .unwrap();
        assert_eq!(stored.verdict, verdict);
        let event = db
            .events_for_message(ACCT, THREAT_VERDICT, "p1@x", 1)
            .unwrap()
            .remove(0);
        let payload = event.payload.unwrap();
        assert!(
            !payload.contains("verify your account"),
            "no bodies in the event"
        );
        assert!(!payload.contains("invoice"), "no filenames in the event");
        assert!(event.acked_at.is_some());
    }

    #[test]
    fn rescan_replaces_level_tags_and_unavailable_drops_the_score() {
        let db = Database::open_memory().unwrap();
        let target = VerdictTarget {
            account_id: ACCT,
            folder: "INBOX",
            uid: 1,
            message_id: Some("m@x"),
            content_fingerprint: None,
            observed_message_ids: &[],
        };
        let (bad, _) = scan_raw(&db, ACCT, EMAIL, &phish("m@x"), &ThreatConfig::default());
        record_verdict(&db, &target, &bad).unwrap();
        record_verdict(&db, &target, &ThreatVerdict::unavailable("clamd down")).unwrap();
        assert!(tags(&db, "m@x").is_empty());
        assert!(db.get_scores(ACCT, "m@x").unwrap().is_empty());
        assert_eq!(
            stored_verdict_for_uid(&db, ACCT, "INBOX", 1)
                .unwrap()
                .unwrap()
                .verdict
                .level,
            Level::Unavailable
        );
    }

    #[test]
    fn stored_verdict_is_found_only_under_the_scanned_uid_or_its_bytes() {
        let db = Database::open_memory().unwrap();
        let raw = phish("m@x");
        let (verdict, scanned) = scan_raw(&db, ACCT, EMAIL, &raw, &ThreatConfig::default());
        record_verdict(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 7,
                message_id: Some("m@x"),
                content_fingerprint: scanned.content_fingerprint.as_deref(),
                observed_message_ids: &[],
            },
            &verdict,
        )
        .unwrap();

        let stored = stored_verdict_for_uid(&db, ACCT, "INBOX", 7)
            .unwrap()
            .unwrap();
        assert_eq!(stored.verdict, verdict);
        assert_eq!((stored.folder.as_str(), stored.uid), ("INBOX", Some(7)));
        assert_eq!(stored.message_id.as_deref(), Some("m@x"));
        assert!(
            stored_verdict_for_uid(&db, ACCT, "INBOX", 9)
                .unwrap()
                .is_none()
        );
        let found = |fp: &str| {
            matching_verdict(&db, ACCT, "INBOX", 9, Some("m@x"), fp)
                .unwrap()
                .map(|s| s.uid)
        };
        assert_eq!(found(&content_fingerprint(&raw)), Some(Some(7)));
        assert_eq!(found(&content_fingerprint(&ordinary("m@x"))), None);
    }

    #[test]
    fn verdict_event_carries_the_fingerprint_beside_the_verdict() {
        let db = Database::open_memory().unwrap();
        let raw = phish("f@x");
        let fp = content_fingerprint(&raw);
        let (verdict, scanned) = scan_raw(&db, ACCT, EMAIL, &raw, &ThreatConfig::default());
        assert_eq!(scanned.content_fingerprint.as_deref(), Some(fp.as_str()));
        let target = |uid, content_fingerprint| VerdictTarget {
            account_id: ACCT,
            folder: "INBOX",
            uid,
            message_id: Some("f@x"),
            content_fingerprint,
            observed_message_ids: &[],
        };
        let payload = |uid| {
            db.latest_event_for_uid(ACCT, THREAT_VERDICT, "INBOX", uid)
                .unwrap()
                .unwrap()
                .payload
                .unwrap()
        };

        record_verdict(&db, &target(3, Some(fp.as_str())), &verdict).unwrap();
        let stored = stored_verdict_for_uid(&db, ACCT, "INBOX", 3)
            .unwrap()
            .unwrap();
        assert_eq!(stored.content_fingerprint.as_deref(), Some(fp.as_str()));
        assert_eq!(stored.verdict, verdict);
        // The payload is the verdict plus one sibling field, so a reader
        // that knows only the verdict still parses it.
        let mut expected = serde_json::to_value(&verdict).unwrap();
        expected["content_fingerprint"] = json!(fp);
        let stored_payload = payload(3);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&stored_payload).unwrap(),
            expected
        );
        assert_eq!(
            serde_json::from_str::<ThreatVerdict>(&stored_payload).unwrap(),
            verdict
        );

        record_verdict(&db, &target(4, None), &verdict).unwrap();
        let legacy = stored_verdict_for_uid(&db, ACCT, "INBOX", 4)
            .unwrap()
            .unwrap();
        assert_eq!(legacy.content_fingerprint, None);
        assert!(!payload(4).contains("content_fingerprint"));
    }

    #[tokio::test]
    async fn new_mail_pass_records_the_fingerprint_of_the_bytes_it_scanned() {
        let db = Database::open_memory().unwrap();
        let mut mbox = FakeMailbox::default();
        mbox.raw.insert(5, ordinary("n@x"));
        let results = scan_new_mail(
            &mut mbox,
            &db,
            &account(),
            "INBOX",
            &[5],
            &ThreatConfig::default(),
        )
        .await;
        results[0].1.as_ref().unwrap();
        let stored = stored_verdict_for_uid(&db, ACCT, "INBOX", 5)
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.content_fingerprint,
            Some(content_fingerprint(&ordinary("n@x")))
        );
    }

    #[test]
    fn rules_touching_quarantine_or_verdicts_are_recognized() {
        for folder in [
            "Envelope/Quarantine",
            " envelope.quarantine ",
            "INBOX/Envelope/Quarantine",
            "INBOX.Envelope.Quarantine/",
        ] {
            assert!(is_quarantine_folder(folder), "{folder}");
        }
        for folder in ["INBOX", "Envelope", "Quarantine", "Envelope/Quarantine/Old"] {
            assert!(!is_quarantine_folder(folder), "{folder}");
        }
        assert!(is_quarantine_rule_name("  envelope THREAT quarantine "));
        assert!(!is_quarantine_rule_name("Envelope threat quarantine 2"));

        let any = MatchExpr::From("*".to_string());
        let tagged = MatchExpr::And(vec![
            any.clone(),
            MatchExpr::Not(Box::new(MatchExpr::HasTag(
                " Threat:Quarantined".to_string(),
            ))),
        ]);
        let inbox = Action::Move("INBOX".to_string());
        assert!(rule_touches_threat_state(&tagged, &inbox));
        assert!(rule_touches_threat_state(
            &any,
            &Action::Move("envelope.quarantine".to_string())
        ));
        assert!(rule_touches_threat_state(
            &any,
            &Action::AddTag("threat:false_positive".to_string())
        ));
        assert!(!rule_touches_threat_state(&any, &inbox));
        // The threat score is the verdict's, so selecting by it is too.
        for score in [
            MatchExpr::ScoreAbove {
                dimension: "threat".to_string(),
                threshold: 50.0,
            },
            MatchExpr::Or(vec![
                any.clone(),
                MatchExpr::ScoreBelow {
                    dimension: " Threat ".to_string(),
                    threshold: 10.0,
                },
            ]),
        ] {
            assert!(rule_touches_threat_state(&score, &inbox), "{score:?}");
        }
        let urgency = MatchExpr::ScoreAbove {
            dimension: "urgent".to_string(),
            threshold: 0.5,
        };
        assert!(!rule_touches_threat_state(&urgency, &inbox));
        let (match_expr, action) = quarantine_rule_json();
        let shipped = StoredRuleAction::parse(&action).unwrap().action;
        let shipped_match: MatchExpr = serde_json::from_str(&match_expr).unwrap();
        assert!(rule_touches_threat_state(&shipped_match, &shipped));
    }

    #[test]
    fn needs_scan_on_missing_or_old_engine() {
        assert!(needs_scan(None));
        let mut v = ThreatVerdict::unavailable("x");
        assert!(!needs_scan(Some(&v)));
        v.engine_version = "rshield-0".to_string();
        assert!(needs_scan(Some(&v)));
        // 1.3.15's verdicts predate the account-set receiving domain.
        v.engine_version = "rshield-1".to_string();
        assert!(needs_scan(Some(&v)));
        assert_eq!(ENGINE_VERSION, "rshield-2");
    }

    #[test]
    fn attachment_gate_honours_malware_tag_bytes_and_mark_safe() {
        let db = Database::open_memory().unwrap();
        let fp = Some("v1:a");
        // Untagged message, clean PDF bytes: allowed.
        assert!(
            attachment_block(
                &db,
                ACCT,
                Some("a@x"),
                fp,
                "r.pdf",
                "application/pdf",
                b"%PDF"
            )
            .unwrap()
            .is_none()
        );
        // Malware-grade bytes are refused even with no verdict on file.
        let block = attachment_block(&db, ACCT, None, None, "r.pdf.exe", "application/pdf", b"MZ")
            .unwrap()
            .unwrap();
        assert_eq!(block.code, ATTACHMENT_BLOCKED);
        // A threat:malware message refuses even innocent-looking bytes.
        db.add_tag(ACCT, "a@x", TAG_MALWARE, Some(3), Some("INBOX"))
            .unwrap();
        let block = attachment_block(
            &db,
            ACCT,
            Some("a@x"),
            fp,
            "r.pdf",
            "application/pdf",
            b"%PDF",
        )
        .unwrap()
        .unwrap();
        assert!(block.reason.contains("threat:malware"));
        // Marked safe: released.
        let target = VerdictTarget {
            account_id: ACCT,
            folder: "INBOX",
            uid: 3,
            message_id: Some("a@x"),
            content_fingerprint: Some("v1:a"),
            observed_message_ids: &[],
        };
        mark_safe(&db, &target, "cli", None).unwrap();
        assert!(
            attachment_block(
                &db,
                ACCT,
                Some("a@x"),
                fp,
                "r.pdf",
                "application/pdf",
                b"%PDF"
            )
            .unwrap()
            .is_none()
        );
        // The mark releases the content it was bound to, even bytes the gate
        // would refuse; an over-cap download has no fingerprint and is never
        // released by it.
        assert!(
            attachment_block(
                &db,
                ACCT,
                Some("a@x"),
                fp,
                "r.pdf.exe",
                "application/pdf",
                b"MZ"
            )
            .unwrap()
            .is_none()
        );
        assert!(
            attachment_block(
                &db,
                ACCT,
                Some("a@x"),
                None,
                "r.pdf.exe",
                "application/pdf",
                b"MZ"
            )
            .unwrap()
            .is_some()
        );
        assert_eq!(tags(&db, "a@x"), vec![TAG_FALSE_POSITIVE]);
        let label = db
            .events_for_message(ACCT, LABEL_APPLIED, "a@x", 1)
            .unwrap()
            .remove(0);
        assert!(label.payload.unwrap().contains(TAG_FALSE_POSITIVE));
    }

    /// Bytes are written under the sanitized name, so the gate checks that
    /// name as well as the original.
    #[test]
    fn attachment_gate_checks_the_name_written_to_disk() {
        let db = Database::open_memory().unwrap();
        for name in ["payload.js\u{1}", "payload.js\u{0}", "payload.js "] {
            let block = attachment_block(&db, ACCT, None, None, name, "text/plain", b"alert(1)")
                .unwrap()
                .unwrap_or_else(|| panic!("{name:?} must be blocked"));
            assert_eq!(block.code, ATTACHMENT_BLOCKED);
        }
        assert!(
            attachment_block(&db, ACCT, None, None, "notes.txt\u{1}", "text/plain", b"hi")
                .unwrap()
                .is_none()
        );
    }

    /// The reader shows a blocked attachment as blocked, so it asks the same
    /// gate the download chokepoint uses, per attachment of the raw message.
    #[test]
    fn blocked_attachments_names_what_the_download_gate_refuses() {
        let db = Database::open_memory().unwrap();
        let raw = b"From: a@example.org\r\nTo: me@example.org\r\nMessage-ID: <m1@x>\r\n\
Subject: s\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n\
--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"notes.pdf\"\r\n\r\n%PDF-1.4\r\n\
--b\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"invoice.pdf.exe\"\r\n\r\nMZharmless\r\n\
--b--\r\n";
        let blocked = blocked_attachments(&db, ACCT, raw).unwrap();
        let names: Vec<&str> = blocked.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["invoice.pdf.exe"]);
        assert_eq!(blocked[0].1.code, ATTACHMENT_BLOCKED);

        // A threat:malware message blocks every attachment, innocent or not.
        db.add_tag(ACCT, "m1@x", TAG_MALWARE, Some(3), Some("INBOX"))
            .unwrap();
        let blocked = blocked_attachments(&db, ACCT, raw).unwrap();
        let names: Vec<&str> = blocked.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["notes.pdf", "invoice.pdf.exe"]);
        assert!(blocked[0].1.reason.contains("threat:malware"));
    }

    /// A clean message that reuses a malware message's Message-ID leaves the
    /// original's attachments blocked: by its tag, and by its own verdict,
    /// found by fingerprint, once a rescan has cleared the tag.
    #[test]
    fn clean_resend_does_not_unblock_original_malware_attachment() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        let original = b"From: IT Desk <it@examp1e.org>\r\nTo: me@example.org\r\n\
Message-ID: <orig@x>\r\nSubject: s\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n\
--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"notes.pdf\"\r\n\r\n%PDF-1.4\r\n\
--b\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"invoice.pdf.exe\"\r\n\r\nMZharmless\r\n\
--b--\r\n";
        let scanned = verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            1,
            Opened::Whole(original),
            &config,
        )
        .unwrap()
        .unwrap();
        assert!(scanned.is_malware());
        let resend = verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            2,
            Opened::Whole(&ordinary("orig@x")),
            &config,
        )
        .unwrap()
        .unwrap();
        assert_eq!(resend.level, Level::Clean);
        assert!(
            tags(&db, "orig@x").contains(&TAG_MALWARE.to_string()),
            "the resend is keyed by its own content and cannot clear the tag"
        );
        assert_eq!(
            blocked_names(&db, original),
            ["notes.pdf", "invoice.pdf.exe"]
        );

        // A rescan of the original that comes back unavailable clears its
        // tags; its malware verdict on file still blocks.
        record_verdict(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 1,
                message_id: Some("orig@x"),
                content_fingerprint: Some(&content_fingerprint(original)),
                observed_message_ids: &[],
            },
            &ThreatVerdict::unavailable("clamd down"),
        )
        .unwrap();
        assert!(!tags(&db, "orig@x").contains(&TAG_MALWARE.to_string()));
        let blocked = blocked_attachments(&db, ACCT, original).unwrap();
        let names: Vec<&str> = blocked.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["notes.pdf", "invoice.pdf.exe"]);
        assert!(
            blocked[0].1.reason.contains("verdict"),
            "{}",
            blocked[0].1.reason
        );
    }

    /// A message with an innocent and a malware-grade attachment, from a
    /// lookalike sender, with the given Message-ID header lines.
    fn two_attachments(message_id_headers: &str) -> Vec<u8> {
        format!(
            "From: IT Desk <it@examp1e.org>\r\nTo: me@example.org\r\n{message_id_headers}\
Subject: s\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n\
--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"notes.pdf\"\r\n\r\n%PDF-1.4\r\n\
--b\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"invoice.pdf.exe\"\r\n\r\nMZharmless\r\n\
--b--\r\n"
        )
        .into_bytes()
    }

    /// The key threat data is stored under for a message without one usable
    /// Message-ID.
    fn fp_key(raw: &[u8]) -> String {
        format!("fp:{}", content_fingerprint(raw))
    }

    fn blocked_names(db: &Database, raw: &[u8]) -> Vec<String> {
        blocked_attachments(db, ACCT, raw)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// The scanner and the attachment gate read the same identity, so a
    /// second Message-ID header cannot split a verdict from its attachments.
    #[test]
    fn two_message_id_headers_block_the_attachments() {
        let db = Database::open_memory().unwrap();
        let dup = two_attachments("Message-ID: <first@x>\r\nMessage-ID: <second@x>\r\n");
        let verdict = verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            2,
            Opened::Whole(&dup),
            &ThreatConfig::default(),
        )
        .unwrap()
        .unwrap();
        assert!(verdict.is_malware());
        assert_eq!(blocked_names(&db, &dup), ["notes.pdf", "invoice.pdf.exe"]);
        assert_eq!(tags(&db, &fp_key(&dup)), [TAG_DANGEROUS, TAG_MALWARE]);
        assert!(tags(&db, "first@x").is_empty() && tags(&db, "second@x").is_empty());
    }

    #[tokio::test]
    async fn empty_message_id_is_quarantined_tagged_and_scored() {
        let db = Database::open_memory().unwrap();
        let header = |mid: &str| format!("Message-ID: <{mid}>\r\n");
        let empty_first = String::from_utf8(phish("real@x"))
            .unwrap()
            .replace(&header("real@x"), "Message-ID:\r\nMessage-ID: <real@x>\r\n")
            .into_bytes();
        let empty_only = String::from_utf8(phish("gone@x"))
            .unwrap()
            .replace(&header("gone@x"), "Message-ID: \r\n")
            .into_bytes();
        let mut mbox = FakeMailbox::default();
        mbox.raw.insert(3, empty_first.clone());
        mbox.raw.insert(4, empty_only.clone());
        let results = scan_new_mail(
            &mut mbox,
            &db,
            &account(),
            "INBOX",
            &[3, 4],
            &ThreatConfig::default(),
        )
        .await;
        for ((uid, result), raw) in results.iter().zip([&empty_first, &empty_only]) {
            let entry = result.as_ref().unwrap();
            assert_eq!(entry.level, Level::Dangerous, "UID {uid}");
            assert_eq!(entry.quarantine, QuarantineOutcome::Tagged, "UID {uid}");
            let key = fp_key(raw);
            assert_eq!(
                tags(&db, &key),
                [TAG_DANGEROUS, TAG_MALWARE, TAG_QUARANTINED],
                "UID {uid}"
            );
            assert_eq!(
                db.get_scores(ACCT, &key).unwrap()[0].value,
                f64::from(entry.score),
                "UID {uid}"
            );
        }
        assert!(tags(&db, "real@x").is_empty());
    }

    #[test]
    fn without_a_message_id_the_gate_checks_the_fingerprint_verdict() {
        let db = Database::open_memory().unwrap();
        let none = two_attachments("");
        let fp = content_fingerprint(&none);
        let clamd = super::super::combine(
            vec![Signal::new("clamd_found", 100, "Eicar").malware()],
            vec![],
            vec![],
            false,
        );
        record_verdict(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 4,
                message_id: None,
                content_fingerprint: Some(&fp),
                observed_message_ids: &[],
            },
            &clamd,
        )
        .unwrap();
        let block = attachment_block(
            &db,
            ACCT,
            None,
            Some(&fp),
            "notes.pdf",
            "application/pdf",
            b"%PDF-1.4",
        )
        .unwrap()
        .expect("the clamd verdict on these bytes blocks every attachment");
        assert!(block.reason.contains("malware"), "{}", block.reason);
        assert_eq!(blocked_names(&db, &none), ["notes.pdf", "invoice.pdf.exe"]);
    }

    fn rule_context_for(db: &Database, uid: u32, mid: &str) -> MessageContext {
        let summary = envelope_email_store::MessageSummary {
            uid,
            message_id: Some(format!("<{mid}>")),
            from_addr: "it@examp1e.org".into(),
            to_addr: EMAIL.into(),
            subject: "s".into(),
            date: None,
            flags: vec![],
            size: 0,
            provider_spam: None,
        };
        crate::rule_exec::build_summary_context(&summary, "INBOX", db, ACCT).unwrap()
    }

    fn threat_tags(ctx: &MessageContext) -> Vec<&str> {
        let mut tags: Vec<&str> = ctx
            .tags
            .iter()
            .map(String::as_str)
            .filter(|t| t.starts_with("threat:"))
            .collect();
        tags.sort_unstable();
        tags
    }

    /// Twins share a Message-ID; each one's rule context carries its own
    /// verdict, whichever was scanned first.
    #[tokio::test]
    async fn rules_see_each_twin_s_own_threat_data() {
        for order in [[10, 11], [11, 10]] {
            let db = Database::open_memory().unwrap();
            let mut mbox = FakeMailbox::default();
            mbox.raw.insert(10, phish("twin@x"));
            mbox.raw.insert(11, ordinary("twin@x"));
            let results = scan_new_mail(
                &mut mbox,
                &db,
                &account(),
                "INBOX",
                &order,
                &ThreatConfig::default(),
            )
            .await;
            let score = |uid| {
                results
                    .iter()
                    .find(|(u, _)| *u == uid)
                    .map(|(_, r)| f64::from(r.as_ref().unwrap().score))
                    .unwrap()
            };

            let bad = rule_context_for(&db, 10, "twin@x");
            assert_eq!(
                bad.scores.get(THREAT_DIMENSION),
                Some(&score(10)),
                "{order:?}"
            );
            assert_eq!(
                threat_tags(&bad),
                [TAG_DANGEROUS, TAG_MALWARE, TAG_QUARANTINED],
                "{order:?}"
            );
            let clean = rule_context_for(&db, 11, "twin@x");
            assert_eq!(
                clean.scores.get(THREAT_DIMENSION),
                Some(&score(11)),
                "{order:?}"
            );
            assert!(
                threat_tags(&clean).is_empty(),
                "{order:?}: {:?}",
                clean.tags
            );
        }
    }

    /// Mark safe on one twin never lowers the other's threat data: rules,
    /// `threat show` tags and the attachment gate still see it as flagged.
    #[tokio::test]
    async fn mark_safe_on_one_twin_leaves_the_other_flagged() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        let a = ordinary("q@x");
        let a_fp = content_fingerprint(&a);
        verdict_on_open(&db, ACCT, EMAIL, "INBOX", 1, Opened::Whole(&a), &config).unwrap();
        let b = two_attachments("Message-ID: <q@x>\r\n");
        let b_fp = content_fingerprint(&b);
        let mut mbox = FakeMailbox::default();
        mbox.raw.insert(2, b.clone());
        let results = scan_new_mail(&mut mbox, &db, &account(), "INBOX", &[2], &config).await;
        let b_entry = results[0].1.as_ref().unwrap();
        assert_eq!(b_entry.quarantine, QuarantineOutcome::Tagged);

        mark_safe(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 1,
                message_id: Some("q@x"),
                content_fingerprint: Some(&a_fp),
                observed_message_ids: &[],
            },
            "reader",
            None,
        )
        .unwrap();

        let ctx = rule_context_for(&db, 2, "q@x");
        assert_eq!(
            ctx.scores.get(THREAT_DIMENSION),
            Some(&f64::from(b_entry.score))
        );
        assert_eq!(
            threat_tags(&ctx),
            [TAG_DANGEROUS, TAG_MALWARE, TAG_QUARANTINED]
        );
        let stored = stored_verdict_for_uid(&db, ACCT, "INBOX", 2)
            .unwrap()
            .unwrap();
        assert_eq!(stored.message_id.as_deref(), Some("q@x"));
        assert_eq!(
            stored.key,
            Some(fp_key(&b)),
            "the reuse is keyed by content"
        );
        assert_eq!(
            tags(&db, stored.key.as_deref().unwrap()),
            [TAG_DANGEROUS, TAG_MALWARE, TAG_QUARANTINED]
        );
        assert_eq!(blocked_names(&db, &b), ["notes.pdf", "invoice.pdf.exe"]);
        assert!(!is_marked_safe(&db, ACCT, stored.key.as_deref().unwrap(), Some(&b_fp)).unwrap());

        let a_ctx = rule_context_for(&db, 1, "q@x");
        assert_eq!(threat_tags(&a_ctx), [TAG_FALSE_POSITIVE]);
        assert_eq!(a_ctx.scores.get(THREAT_DIMENSION), Some(&0.0));
    }

    /// The gate reads a bounded window of a message's verdicts. When the
    /// window is full and holds no malware verdict for these bytes, it cannot
    /// rule one out (here, clean rescans of the same content pushed it out),
    /// so it refuses.
    #[test]
    fn attachment_gate_fails_closed_past_the_verdict_history_it_reads() {
        let db = Database::open_memory().unwrap();
        let malware = super::super::combine(
            vec![Signal::new("clamd_found", 100, "Eicar").malware()],
            vec![],
            vec![],
            false,
        );
        let clean = super::super::combine(vec![], vec![], vec![], false);
        let target = |uid| VerdictTarget {
            account_id: ACCT,
            folder: "INBOX",
            uid,
            message_id: Some("long@x"),
            content_fingerprint: Some("v1:orig"),
            observed_message_ids: &[],
        };
        record_verdict_event(&db, &target(1), Some("long@x"), &malware).unwrap();
        let gate = || {
            attachment_block(
                &db,
                ACCT,
                Some("long@x"),
                Some("v1:orig"),
                "r.pdf",
                "application/pdf",
                b"%PDF",
            )
            .unwrap()
        };
        assert!(gate().unwrap().reason.contains("verdict"));
        for uid in 2..=FINGERPRINT_SEARCH_LIMIT as u32 + 1 {
            record_verdict_event(&db, &target(uid), Some("long@x"), &clean).unwrap();
        }
        let block = gate().expect("refused when the history is longer than the window");
        assert!(
            block.reason.contains("more stored verdicts"),
            "{}",
            block.reason
        );
    }

    /// A part-by-part read of an over-cap message passes no raw bytes: there
    /// is nothing complete to scan, so the stored verdict (if any) is returned
    /// and nothing is recorded.
    #[test]
    fn open_without_raw_bytes_returns_the_stored_verdict_and_never_scans() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        assert!(config.enabled && config.on_read);
        let big = Opened::Parts {
            message_id: Some("big@x"),
        };
        assert!(
            verdict_on_open(&db, ACCT, EMAIL, "INBOX", 2379, big, &config)
                .unwrap()
                .is_none()
        );
        assert!(
            stored_verdict_for_uid(&db, ACCT, "INBOX", 2379)
                .unwrap()
                .is_none(),
            "a partial read must not record a verdict"
        );

        let target = VerdictTarget {
            account_id: ACCT,
            folder: "INBOX",
            uid: 2379,
            message_id: Some("big@x"),
            content_fingerprint: None,
            observed_message_ids: &[],
        };
        let stored = ThreatVerdict::unavailable("clamd down");
        record_verdict(&db, &target, &stored).unwrap();
        assert_eq!(
            verdict_on_open(&db, ACCT, EMAIL, "INBOX", 2379, big, &config).unwrap(),
            Some(stored)
        );
    }

    /// An over-cap message at a reused UID: the verdict there is for the
    /// message that held it before, so the reader shows none.
    #[test]
    fn open_without_raw_bytes_shows_no_verdict_left_by_another_message() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        let old = b"Message-ID: <old@x>\r\nFrom: a@example.test\r\nTo: me@example.org\r\n\
Subject: s\r\n\r\nhi\r\n";
        let scanned = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 7, Opened::Whole(old), &config)
            .unwrap()
            .unwrap();
        let open = |message_id| {
            let parts = Opened::Parts { message_id };
            verdict_on_open(&db, ACCT, EMAIL, "INBOX", 7, parts, &config).unwrap()
        };

        assert_eq!(open(Some("big@x")), None);
        assert_eq!(open(None), None);
        assert_eq!(open(Some("<old@x>")), Some(scanned));
    }

    /// A verdict stored before fingerprints is shown, and rescanned when the
    /// message is opened with the engine on.
    #[test]
    fn legacy_verdict_is_rescanned_on_open() {
        let db = Database::open_memory().unwrap();
        let raw = phish("legacy@x");
        let clean = super::super::combine(vec![], vec!["sender".into()], vec![], false);
        record_verdict(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 4,
                message_id: Some("legacy@x"),
                content_fingerprint: None,
                observed_message_ids: &[],
            },
            &clean,
        )
        .unwrap();
        let off = ThreatConfig {
            on_read: false,
            ..ThreatConfig::default()
        };
        assert_eq!(
            verdict_on_open(&db, ACCT, EMAIL, "INBOX", 4, Opened::Whole(&raw), &off).unwrap(),
            Some(clean)
        );

        let config = ThreatConfig::default();
        let opened = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 4, Opened::Whole(&raw), &config)
            .unwrap()
            .unwrap();
        assert_eq!(opened.level, Level::Dangerous);
        let stored = stored_verdict_for_uid(&db, ACCT, "INBOX", 4)
            .unwrap()
            .unwrap();
        assert_eq!(stored.verdict, opened);
        assert_eq!(stored.content_fingerprint, Some(content_fingerprint(&raw)));
    }

    /// A second message reusing a scanned Message-ID is scanned on open, and
    /// the first keeps its own verdict.
    #[test]
    fn open_same_message_id_different_bytes_scans() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        let first = verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            1,
            Opened::Whole(&ordinary("dup@x")),
            &config,
        )
        .unwrap()
        .unwrap();
        assert_eq!(first.level, Level::Clean);

        let second = verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            2,
            Opened::Whole(&phish("dup@x")),
            &config,
        )
        .unwrap()
        .unwrap();
        assert_eq!(second.level, Level::Dangerous);
        assert!(second.is_malware());
        assert_eq!(
            tags(&db, &fp_key(&phish("dup@x"))),
            vec![TAG_DANGEROUS, TAG_MALWARE],
            "the second message is tagged as itself"
        );
        assert!(
            tags(&db, "dup@x").is_empty(),
            "the first keeps its own tags"
        );

        let again = verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            1,
            Opened::Whole(&ordinary("dup@x")),
            &config,
        )
        .unwrap()
        .unwrap();
        assert_eq!(again, first);
    }

    /// The same bytes under a new folder/UID (moved, or copied back) reuse the
    /// verdict and its Mark safe without a rescan, and the verdict is recorded
    /// under the new UID too.
    #[test]
    fn moved_copy_reuses_verdict_and_mark_safe_without_rescan() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        let raw = phish("mv@x");
        let fp = content_fingerprint(&raw);
        let verdict = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 1, Opened::Whole(&raw), &config)
            .unwrap()
            .unwrap();
        mark_safe(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 1,
                message_id: Some("mv@x"),
                content_fingerprint: Some(&fp),
                observed_message_ids: &[],
            },
            "cli",
            None,
        )
        .unwrap();

        let moved = verdict_on_open(&db, ACCT, EMAIL, "Archive", 9, Opened::Whole(&raw), &config)
            .unwrap()
            .unwrap();
        assert_eq!(
            moved, verdict,
            "computed_at included: nothing was rescanned"
        );
        let copy = stored_verdict_for_uid(&db, ACCT, "Archive", 9)
            .unwrap()
            .expect("the reused verdict is recorded under the new UID");
        assert_eq!(copy.verdict, verdict);
        assert_eq!(copy.message_id.as_deref(), Some("mv@x"));
        assert_eq!(copy.content_fingerprint.as_deref(), Some(fp.as_str()));
        assert_eq!(
            db.events_for_message(ACCT, THREAT_VERDICT, "mv@x", 10)
                .unwrap()
                .len(),
            2,
            "one scan and one copy"
        );
        assert_eq!(tags(&db, "mv@x"), vec![TAG_FALSE_POSITIVE]);
        assert_eq!(db.get_scores(ACCT, "mv@x").unwrap()[0].value, 0.0);
        assert!(is_marked_safe(&db, ACCT, "mv@x", Some(&fp)).unwrap());
    }

    /// Mark safe binds to the bytes that were marked. A later message that
    /// reuses the Message-ID is scanned, scored, tagged and quarantined as
    /// itself.
    #[tokio::test]
    async fn resend_of_marked_safe_message_id_with_different_content_is_scanned_and_not_released() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        assert_eq!(config.quarantine, Quarantine::Tag);
        let original = ordinary("reuse@x");
        let original_fp = content_fingerprint(&original);
        verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            1,
            Opened::Whole(&original),
            &config,
        )
        .unwrap();
        mark_safe(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 1,
                message_id: Some("reuse@x"),
                content_fingerprint: Some(&original_fp),
                observed_message_ids: &[],
            },
            "reader",
            None,
        )
        .unwrap();

        let resend = phish("reuse@x");
        let mut mbox = FakeMailbox::default();
        mbox.raw.insert(2, resend.clone());
        let results = scan_new_mail(&mut mbox, &db, &account(), "INBOX", &[2], &config).await;
        let entry = results[0].1.as_ref().unwrap();
        assert_eq!(entry.level, Level::Dangerous);
        assert_eq!(entry.quarantine, QuarantineOutcome::Tagged);
        // The resend is scored and tagged as itself, under its own key; the
        // marked original keeps score 0 and its mark.
        let resend_key = fp_key(&resend);
        assert_eq!(
            db.get_scores(ACCT, &resend_key).unwrap()[0].value,
            f64::from(entry.score)
        );
        assert_eq!(
            tags(&db, &resend_key),
            vec![TAG_DANGEROUS, TAG_MALWARE, TAG_QUARANTINED]
        );
        assert_eq!(db.get_scores(ACCT, "reuse@x").unwrap()[0].value, 0.0);
        assert_eq!(tags(&db, "reuse@x"), vec![TAG_FALSE_POSITIVE]);
        let opened = verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            2,
            Opened::Whole(&resend),
            &config,
        )
        .unwrap()
        .unwrap();
        assert_eq!(opened.level, Level::Dangerous);
        let resend_fp = content_fingerprint(&resend);
        assert!(!is_marked_safe(&db, ACCT, "reuse@x", Some(&resend_fp)).unwrap());
        assert!(!is_marked_safe(&db, ACCT, &resend_key, Some(&resend_fp)).unwrap());
        assert!(is_marked_safe(&db, ACCT, "reuse@x", Some(&original_fp)).unwrap());
        let gate = |fp: &str| {
            attachment_block(
                &db,
                ACCT,
                Some("reuse@x"),
                Some(fp),
                "notes.pdf",
                "application/pdf",
                b"%PDF",
            )
            .unwrap()
        };
        assert!(
            gate(&resend_fp).is_some(),
            "the resend's attachments stay blocked"
        );
        assert!(
            gate(&original_fp).is_none(),
            "the marked original's are released"
        );
    }

    /// A resend with the marked message's header fields and body, plus text
    /// after a form-feed-only line that the reader shows as body, is a
    /// different message: scanned, scored and gated as itself.
    #[tokio::test]
    async fn resend_of_marked_safe_content_with_text_after_a_form_feed_line_is_not_released() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        let original = ordinary("ff@x");
        let original_fp = content_fingerprint(&original);
        verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            1,
            Opened::Whole(&original),
            &config,
        )
        .unwrap();
        mark_safe(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 1,
                message_id: Some("ff@x"),
                content_fingerprint: Some(&original_fp),
                observed_message_ids: &[],
            },
            "reader",
            None,
        )
        .unwrap();

        let resend = String::from_utf8(original.clone())
            .unwrap()
            .replace(
                "Subject: Lunch\r\n\r\n",
                "Subject: Lunch\r\n\x0c\r\nVerify your mailbox at http://examp1e.org/login\r\n\r\n",
            )
            .into_bytes();
        let shown = ThreatInput::from_raw(&resend, EMAIL).unwrap().text.unwrap();
        assert!(shown.contains("examp1e.org"), "{shown:?}");

        let mut mbox = FakeMailbox::default();
        mbox.raw.insert(2, resend.clone());
        let results = scan_new_mail(&mut mbox, &db, &account(), "INBOX", &[2], &config).await;
        let entry = results[0].1.as_ref().unwrap();
        assert!(entry.score > 0, "{entry:?}");
        let resend_fp = content_fingerprint(&resend);
        let resend_key = fp_key(&resend);
        assert!(!is_marked_safe(&db, ACCT, "ff@x", Some(&resend_fp)).unwrap());
        assert!(!is_marked_safe(&db, ACCT, &resend_key, Some(&resend_fp)).unwrap());
        assert_eq!(
            db.get_scores(ACCT, &resend_key).unwrap()[0].value,
            f64::from(entry.score)
        );
        assert!(!tags(&db, &resend_key).contains(&TAG_FALSE_POSITIVE.to_string()));
        assert!(is_marked_safe(&db, ACCT, "ff@x", Some(&original_fp)).unwrap());

        // Opened elsewhere, the resend is scanned rather than given the
        // marked original's verdict.
        let opened = verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "Archive",
            3,
            Opened::Whole(&resend),
            &config,
        )
        .unwrap()
        .unwrap();
        assert_eq!(opened.score, entry.score);
    }

    /// A Subject line only the raw header reader files as Subject changes the
    /// fingerprint, so a message carrying one never reuses the verdict of the
    /// message without it.
    #[test]
    fn a_subject_line_only_the_raw_reader_files_never_reuses_a_verdict() {
        let config = ThreatConfig::default();
        let original = ordinary("s@x");
        for (uid, prefix) in [(2, "\u{a0}"), (3, "\u{2003}"), (4, "\x0b")] {
            let db = Database::open_memory().unwrap();
            verdict_on_open(
                &db,
                ACCT,
                EMAIL,
                "INBOX",
                1,
                Opened::Whole(&original),
                &config,
            )
            .unwrap();
            let twin = String::from_utf8(original.clone())
                .unwrap()
                .replacen(
                    "Subject: Lunch",
                    &format!(
                        "{prefix}Subject: urgent verify your account immediately\r\nSubject: Lunch"
                    ),
                    1,
                )
                .into_bytes();
            let twin_fp = content_fingerprint(&twin);
            assert_ne!(twin_fp, content_fingerprint(&original), "{prefix:?}");
            assert!(
                matching_verdict(&db, ACCT, "INBOX", uid, Some("s@x"), &twin_fp)
                    .unwrap()
                    .is_none(),
                "{prefix:?}"
            );
            verdict_on_open(
                &db,
                ACCT,
                EMAIL,
                "INBOX",
                uid,
                Opened::Whole(&twin),
                &config,
            )
            .unwrap();
            let own = stored_verdict_for_uid(&db, ACCT, "INBOX", uid)
                .unwrap()
                .unwrap();
            assert_eq!(own.content_fingerprint.as_deref(), Some(twin_fp.as_str()));
        }
    }

    /// A verdict stored without a content fingerprint judged no known bytes:
    /// it is shown when nothing scans, but never reused or marked safe.
    #[test]
    fn a_verdict_without_a_fingerprint_is_shown_but_never_reused_or_marked() {
        let db = Database::open_memory().unwrap();
        let raw = ordinary("u@x");
        let fp = content_fingerprint(&raw);
        let unbound = VerdictTarget {
            account_id: ACCT,
            folder: "INBOX",
            uid: 1,
            message_id: Some("u@x"),
            content_fingerprint: None,
            observed_message_ids: &[],
        };
        let clean = crate::threat::combine(vec![], vec![], vec![], false);
        record_verdict(&db, &unbound, &clean).unwrap();
        assert!(
            matching_verdict(&db, ACCT, "INBOX", 1, Some("u@x"), &fp)
                .unwrap()
                .is_none()
        );

        let off = ThreatConfig {
            enabled: false,
            ..ThreatConfig::default()
        };
        let shown =
            verdict_on_open(&db, ACCT, EMAIL, "INBOX", 1, Opened::Whole(&raw), &off).unwrap();
        assert_eq!(shown, Some(clean));

        verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            1,
            Opened::Whole(&raw),
            &ThreatConfig::default(),
        )
        .unwrap();
        let own = stored_verdict_for_uid(&db, ACCT, "INBOX", 1)
            .unwrap()
            .unwrap();
        assert_eq!(own.content_fingerprint.as_deref(), Some(fp.as_str()));
    }

    /// A Message-ID spelling another message's fingerprint key is unusable,
    /// so that message's threat data never lands under the other's key.
    #[tokio::test]
    async fn message_id_spelling_a_fingerprint_key_does_not_share_that_key() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        let victim = String::from_utf8(ordinary("v@x"))
            .unwrap()
            .replace("Message-ID: <v@x>\r\n", "")
            .into_bytes();
        let victim_key = fp_key(&victim);
        let forged = phish(&victim_key);
        let mut mbox = FakeMailbox::default();
        mbox.raw.insert(1, forged.clone());
        mbox.raw.insert(2, victim.clone());
        scan_new_mail(&mut mbox, &db, &account(), "INBOX", &[1, 2], &config).await;

        assert!(
            !tags(&db, &victim_key).contains(&TAG_MALWARE.to_string()),
            "{:?}",
            tags(&db, &victim_key)
        );
        assert_eq!(
            tags(&db, &fp_key(&forged)),
            vec![TAG_DANGEROUS, TAG_MALWARE, TAG_QUARANTINED]
        );
        let victim_fp = content_fingerprint(&victim);
        let gate = attachment_block(
            &db,
            ACCT,
            None,
            Some(&victim_fp),
            "notes.pdf",
            "application/pdf",
            b"%PDF",
        )
        .unwrap();
        assert!(gate.is_none(), "{gate:?}");
    }

    /// The verdict stored at a folder/UID gives threat data only to the
    /// message it judged: after a UIDVALIDITY reset, another message at that
    /// UID shows none.
    #[tokio::test]
    async fn bound_threat_ignores_a_verdict_for_another_message_at_the_uid() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        let before = phish("before@x");
        let mut mbox = FakeMailbox::default();
        mbox.raw.insert(5, before.clone());
        scan_new_mail(&mut mbox, &db, &account(), "INBOX", &[5], &config).await;
        let tag_names = |seen: Seen<'_>| -> Vec<String> {
            let mut names: Vec<String> = bound_threat(&db, ACCT, "INBOX", 5, seen)
                .unwrap()
                .tags
                .into_iter()
                .map(|t| t.tag)
                .collect();
            names.sort();
            names
        };
        let before_fp = content_fingerprint(&before);
        let flagged = vec![TAG_DANGEROUS, TAG_MALWARE, TAG_QUARANTINED];
        assert_eq!(tag_names(Seen::Bytes(Some(&before_fp))), flagged);
        assert_eq!(tag_names(Seen::MessageId(Some("before@x"))), flagged);

        let after = ordinary("after@x");
        let after_fp = content_fingerprint(&after);
        for seen in [
            Seen::Bytes(Some(&after_fp)),
            Seen::Bytes(None),
            Seen::MessageId(Some("after@x")),
            Seen::MessageId(None),
        ] {
            let bound = bound_threat(&db, ACCT, "INBOX", 5, seen).unwrap();
            assert!(bound.tags.is_empty(), "{seen:?}: {:?}", bound.tags);
            assert!(bound.score.is_none(), "{seen:?}");
        }
        let (tags, _) =
            shown_tags_and_scores(&db, ACCT, "INBOX", 5, "after@x", Some(&after)).unwrap();
        assert!(tags.is_empty(), "{tags:?}");
    }

    /// A tag view shows the message's own threat data: a twin of a marked
    /// message never shows the mark, and a UID without a verdict shows none.
    #[tokio::test]
    async fn tag_view_of_a_twin_does_not_show_the_marked_original_s_threat_tags() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        let original = ordinary("twin@x");
        let original_fp = content_fingerprint(&original);
        verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            1,
            Opened::Whole(&original),
            &config,
        )
        .unwrap();
        mark_safe(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 1,
                message_id: Some("twin@x"),
                content_fingerprint: Some(&original_fp),
                observed_message_ids: &[],
            },
            "reader",
            None,
        )
        .unwrap();
        db.add_tag(ACCT, "twin@x", "work", Some(1), Some("INBOX"))
            .unwrap();
        let mut mbox = FakeMailbox::default();
        mbox.raw.insert(2, phish("twin@x"));
        scan_new_mail(&mut mbox, &db, &account(), "INBOX", &[2], &config).await;

        let shown = |uid: u32| {
            let (tags, scores) =
                shown_tags_and_scores(&db, ACCT, "INBOX", uid, "twin@x", None).unwrap();
            let mut tags: Vec<String> = tags.into_iter().map(|t| t.tag).collect();
            tags.sort();
            let threat: Vec<f64> = scores
                .iter()
                .filter(|s| s.dimension == THREAT_DIMENSION)
                .map(|s| s.value)
                .collect();
            (tags, threat)
        };
        let (twin_tags, twin_threat) = shown(2);
        assert_eq!(
            twin_tags,
            vec![TAG_DANGEROUS, TAG_MALWARE, TAG_QUARANTINED, "work"]
        );
        assert_eq!(twin_threat.len(), 1);
        assert!(twin_threat[0] > 0.0, "{twin_threat:?}");
        assert_eq!(
            shown(1),
            (
                vec![TAG_FALSE_POSITIVE.to_string(), "work".to_string()],
                vec![0.0]
            )
        );
        assert_eq!(shown(9), (vec!["work".to_string()], vec![]));
    }

    /// Mark safe as stored before fingerprints: the tag and a `label_applied`
    /// event without one.
    fn legacy_mark(db: &Database, mid: &str, folder: &str, uid: u32) {
        db.add_tag(
            ACCT,
            mid,
            TAG_FALSE_POSITIVE,
            Some(i64::from(uid)),
            Some(folder),
        )
        .unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        db.insert_event(&Event {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: ACCT.to_string(),
            event_type: LABEL_APPLIED.to_string(),
            folder: folder.to_string(),
            uid: Some(i64::from(uid)),
            message_id: Some(mid.to_string()),
            from_addr: None,
            subject: None,
            snippet: None,
            payload: Some(json!({"label": TAG_FALSE_POSITIVE, "source": "cli"}).to_string()),
            idempotency_key: None,
            secure_pending: false,
            acked_at: Some(now.clone()),
            created_at: now,
        })
        .unwrap();
    }

    #[tokio::test]
    async fn legacy_mark_safe_is_not_honoured() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        legacy_mark(&db, "old@x", "INBOX", 1);
        let raw = phish("old@x");

        let opened = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 5, Opened::Whole(&raw), &config)
            .unwrap()
            .unwrap();
        assert_eq!(opened.level, Level::Dangerous);
        assert_eq!(
            db.get_scores(ACCT, "old@x").unwrap()[0].value,
            f64::from(opened.score)
        );
        assert_eq!(
            tags(&db, "old@x"),
            vec![TAG_DANGEROUS, TAG_FALSE_POSITIVE, TAG_MALWARE]
        );
        assert!(!is_marked_safe(&db, ACCT, "old@x", Some(&content_fingerprint(&raw))).unwrap());

        let mut mbox = FakeMailbox::default();
        mbox.raw.insert(6, raw.clone());
        let results = scan_new_mail(&mut mbox, &db, &account(), "INBOX", &[6], &config).await;
        assert_eq!(
            results[0].1.as_ref().unwrap().quarantine,
            QuarantineOutcome::Tagged
        );

        let unbound = VerdictTarget {
            account_id: ACCT,
            folder: "INBOX",
            uid: 5,
            message_id: Some("old@x"),
            content_fingerprint: None,
            observed_message_ids: &[],
        };
        let err = mark_safe(&db, &unbound, "cli", None).unwrap_err();
        assert!(format!("{err:#}").contains(RESCAN_REQUIRED), "{err:#}");
        assert!(tags(&db, "old@x").contains(&TAG_QUARANTINED.to_string()));
    }

    /// The one legacy mark that still applies: opening the message at the
    /// folder/UID it was made on binds it to the bytes there.
    #[test]
    fn legacy_mark_safe_at_the_same_slot_is_rebound_on_open() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig::default();
        legacy_mark(&db, "slot@x", "INBOX", 3);
        let raw = phish("slot@x");
        let fp = content_fingerprint(&raw);

        let opened = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 3, Opened::Whole(&raw), &config)
            .unwrap()
            .unwrap();
        assert_eq!(opened.level, Level::Dangerous, "the engine's view is kept");
        assert!(is_marked_safe(&db, ACCT, "slot@x", Some(&fp)).unwrap());
        assert_eq!(tags(&db, "slot@x"), vec![TAG_FALSE_POSITIVE]);
        assert_eq!(db.get_scores(ACCT, "slot@x").unwrap()[0].value, 0.0);
        let other = content_fingerprint(&ordinary("slot@x"));
        assert!(!is_marked_safe(&db, ACCT, "slot@x", Some(&other)).unwrap());
    }

    /// A message scanned and marked safe before fingerprints, at the slot it
    /// is opened from: the mark carries over to its content, wherever its
    /// threat data is now keyed.
    #[test]
    fn legacy_verdict_and_mark_at_the_same_slot_are_rebound_on_open() {
        let db = Database::open_memory().unwrap();
        let raw = phish("both@x");
        let (legacy, _) = scan_raw(&db, ACCT, EMAIL, &raw, &ThreatConfig::default());
        record_verdict(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 3,
                message_id: Some("both@x"),
                content_fingerprint: None,
                observed_message_ids: &[],
            },
            &legacy,
        )
        .unwrap();
        legacy_mark(&db, "both@x", "INBOX", 3);

        verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            3,
            Opened::Whole(&raw),
            &ThreatConfig::default(),
        )
        .unwrap()
        .unwrap();
        let stored = stored_verdict_for_uid(&db, ACCT, "INBOX", 3)
            .unwrap()
            .unwrap();
        let key = stored.key.as_deref().unwrap();
        let fp = content_fingerprint(&raw);
        assert!(is_marked_safe(&db, ACCT, key, Some(&fp)).unwrap());
        assert_eq!(tags(&db, key), vec![TAG_FALSE_POSITIVE]);
        assert_eq!(db.get_scores(ACCT, key).unwrap()[0].value, 0.0);
        assert!(
            blocked_names(&db, &raw).is_empty(),
            "released by the rebound mark"
        );
    }

    /// A malware tag stored before fingerprints keeps blocking the message's
    /// attachments after the upgrade, before it is rescanned.
    #[test]
    fn legacy_malware_tag_blocks_until_the_message_is_rescanned() {
        let db = Database::open_memory().unwrap();
        let raw = two_attachments("Message-ID: <old@x>\r\n");
        let (legacy, _) = scan_raw(&db, ACCT, EMAIL, &raw, &ThreatConfig::default());
        assert!(legacy.is_malware());
        record_verdict(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 1,
                message_id: Some("old@x"),
                content_fingerprint: None,
                observed_message_ids: &[],
            },
            &legacy,
        )
        .unwrap();
        assert_eq!(blocked_names(&db, &raw), ["notes.pdf", "invoice.pdf.exe"]);
    }

    #[test]
    fn marked_safe_message_rescans_to_score_zero_without_tags() {
        let db = Database::open_memory().unwrap();
        let fp = content_fingerprint(&phish("s@x"));
        let target = VerdictTarget {
            account_id: ACCT,
            folder: "INBOX",
            uid: 1,
            message_id: Some("s@x"),
            content_fingerprint: Some(&fp),
            observed_message_ids: &[],
        };
        mark_safe(&db, &target, "cli", None).unwrap();
        let (bad, _) = scan_raw(&db, ACCT, EMAIL, &phish("s@x"), &ThreatConfig::default());
        record_verdict(&db, &target, &bad).unwrap();
        assert_eq!(tags(&db, "s@x"), vec![TAG_FALSE_POSITIVE]);
        assert_eq!(db.get_scores(ACCT, "s@x").unwrap()[0].value, 0.0);
    }

    #[tokio::test]
    async fn move_quarantine_runs_the_shipped_rule_attributed_to_envelope_threat() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig {
            quarantine: Quarantine::Move,
            ..ThreatConfig::default()
        };
        let mut mbox = FakeMailbox::default();
        mbox.raw.insert(7, phish("q@x"));
        mbox.raw.insert(8, ordinary("o@x"));
        let results = scan_new_mail(&mut mbox, &db, &account(), "INBOX", &[7, 8], &config).await;
        let entries: Vec<PassEntry> = results.into_iter().map(|(_, r)| r.unwrap()).collect();
        assert_eq!(entries[0].quarantine, QuarantineOutcome::Moved);
        assert_eq!(entries[1].level, Level::Clean);
        assert_eq!(entries[1].quarantine, QuarantineOutcome::NotApplied);
        assert_eq!(
            mbox.calls,
            vec![
                format!("create {QUARANTINE_FOLDER}"),
                format!("move INBOX/7 -> {QUARANTINE_FOLDER}"),
            ]
        );

        let rule = db
            .find_rule_by_name(ACCT, QUARANTINE_RULE_NAME)
            .unwrap()
            .unwrap();
        let (m, a) = quarantine_rule_json();
        assert_eq!(
            (rule.match_expr.as_str(), rule.action.as_str()),
            (m.as_str(), a.as_str())
        );
        let (agent, taken): (Option<String>, String) = db
            .conn()
            .query_row(
                "SELECT agent_id, action_taken FROM action_log WHERE action_type = 'move'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(agent.as_deref(), Some(THREAT_AGENT_ID));
        assert!(taken.contains(QUARANTINE_RULE_NAME));
        assert!(tags(&db, "q@x").contains(&TAG_QUARANTINED.to_string()));

        // Replaying the pass never moves twice.
        let again = scan_new_mail(&mut mbox, &db, &account(), "INBOX", &[7], &config).await;
        assert_eq!(
            again[0].1.as_ref().unwrap().quarantine,
            QuarantineOutcome::Moved
        );
        assert_eq!(
            mbox.calls.iter().filter(|c| c.starts_with("move")).count(),
            1
        );
    }

    #[tokio::test]
    async fn content_the_engine_holds_is_held_in_any_folder() {
        let db = Database::open_memory().unwrap();
        let config = ThreatConfig {
            quarantine: Quarantine::Move,
            ..ThreatConfig::default()
        };
        let quarantined = phish("q@x");
        let mut mbox = FakeMailbox::default();
        mbox.raw.insert(7, quarantined.clone());
        let r = scan_new_mail(&mut mbox, &db, &account(), "INBOX", &[7], &config).await;
        assert_eq!(
            r[0].1.as_ref().unwrap().quarantine,
            QuarantineOutcome::Moved
        );

        // Gmail also lists the message in [Gmail]/All Mail, under another
        // UID: the same bytes, seen from another folder.
        let held = held_reason(&db, ACCT, Some(&quarantined), Some("<q@x>")).unwrap();
        assert!(
            held.as_deref()
                .is_some_and(|r| r.contains(TAG_QUARANTINED) || r.contains(TAG_MALWARE)),
            "{held:?}"
        );
        // Over the fetch cap only the header Message-ID is known.
        assert!(
            held_reason(&db, ACCT, None, Some("<q@x>"))
                .unwrap()
                .is_some()
        );
        // Other content under the same Message-ID is held as well.
        assert!(
            held_reason(&db, ACCT, Some(&ordinary("q@x")), None)
                .unwrap()
                .is_some()
        );
        // Mail the engine does not hold, scanned or not, is not held.
        assert_eq!(
            held_reason(&db, ACCT, Some(&ordinary("o@x")), None).unwrap(),
            None
        );
        // With neither bytes nor a usable Message-ID nothing can be checked.
        assert!(held_reason(&db, ACCT, None, None).unwrap().is_some());

        // The stored verdict on these bytes holds them without the tags.
        for tag in [TAG_DANGEROUS, TAG_MALWARE, TAG_QUARANTINED] {
            db.remove_tag(ACCT, "q@x", tag).unwrap();
        }
        let held = held_reason(&db, ACCT, Some(&quarantined), None).unwrap();
        assert!(
            held.as_deref().is_some_and(|r| r.contains("verdict")),
            "{held:?}"
        );

        // Mark safe on these bytes releases them.
        mark_safe(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: QUARANTINE_FOLDER,
                uid: 7,
                message_id: Some("q@x"),
                content_fingerprint: Some(&content_fingerprint(&quarantined)),
                observed_message_ids: &[],
            },
            "reader",
            None,
        )
        .unwrap();
        assert_eq!(
            held_reason(&db, ACCT, Some(&quarantined), None).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn suspicious_never_moves_and_tag_mode_never_moves() {
        let db = Database::open_memory().unwrap();
        let mut mbox = FakeMailbox::default();
        // Anchor mismatch (35) alone: suspicious.
        mbox.raw.insert(
            1,
            b"Message-ID: <s1@x>\r\nFrom: a@partner.example\r\nContent-Type: text/html\r\n\r\n\
              <a href=\"https://evil.example/\">https://bank.example/</a>"
                .to_vec(),
        );
        mbox.raw.insert(2, phish("t2@x"));
        let move_cfg = ThreatConfig {
            quarantine: Quarantine::Move,
            ..ThreatConfig::default()
        };
        let r = scan_new_mail(&mut mbox, &db, &account(), "INBOX", &[1], &move_cfg).await;
        let entry = r[0].1.as_ref().unwrap();
        assert_eq!(entry.level, Level::Suspicious);
        assert_eq!(entry.quarantine, QuarantineOutcome::NotApplied);

        let r = scan_new_mail(
            &mut mbox,
            &db,
            &account(),
            "INBOX",
            &[2],
            &ThreatConfig::default(),
        )
        .await;
        assert_eq!(
            r[0].1.as_ref().unwrap().quarantine,
            QuarantineOutcome::Tagged
        );
        assert!(mbox.calls.is_empty(), "{:?}", mbox.calls);
    }

    #[test]
    fn stats_report_rates_over_latest_verdicts() {
        let db = Database::open_memory().unwrap();
        let t = |mid: &'static str| VerdictTarget {
            account_id: ACCT,
            folder: "INBOX",
            uid: 1,
            message_id: Some(mid),
            content_fingerprint: None,
            observed_message_ids: &[],
        };
        let (bad, _) = scan_raw(&db, ACCT, EMAIL, &phish("a@x"), &ThreatConfig::default());
        let (good, _) = scan_raw(&db, ACCT, EMAIL, &ordinary("b@x"), &ThreatConfig::default());
        record_verdict(&db, &t("a@x"), &good).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        record_verdict(&db, &t("a@x"), &bad).unwrap();
        record_verdict(&db, &t("b@x"), &good).unwrap();
        record_verdict(&db, &t("c@x"), &ThreatVerdict::unavailable("x")).unwrap();
        let s = stats(&db, ACCT).unwrap();
        assert_eq!(
            (s.scanned, s.clean, s.dangerous, s.unavailable, s.malware),
            (3, 1, 1, 1, 1)
        );
        assert!((s.non_clean_rate - 1.0 / 3.0).abs() < 1e-9);
        assert!((s.unavailable_rate - 1.0 / 3.0).abs() < 1e-9);
    }
    struct ListedDns;

    impl super::super::reputation::DnsResolver for ListedDns {
        fn lookup_a(&self, fqdn: &str) -> Result<super::super::reputation::DnsAnswer, String> {
            use super::super::reputation::DnsAnswer;
            Ok(if fqdn.starts_with("examp1e.org.") {
                DnsAnswer::A(vec![std::net::Ipv4Addr::new(127, 0, 1, 4)])
            } else {
                DnsAnswer::NoRecords
            })
        }
    }

    #[test]
    fn prepare_input_takes_the_receiving_domain_from_the_account() {
        let db = Database::open_memory().unwrap();
        let account = db
            .create_account(
                "me",
                EMAIL,
                "pw",
                "smtp.example.org",
                587,
                "imap.example.org",
                993,
                "passphrase",
            )
            .unwrap();
        let raw = "Authentication-Results: mx.example.org; dmarc=pass header.from=bank.example\r\n\
                   Received: from out.bank.example (out.bank.example [192.0.2.1]) by mx.example.org with ESMTPS id q; Mon, 21 Sep 2026 10:00:00 +0000\r\n\
                   Message-ID: <r1@x>\r\nFrom: Bank <alerts@bank.example>\r\nTo: me@example.org\r\n\
                   Subject: s\r\n\r\nhi\r\n";
        let prepared = |account_id: &str, config: &ThreatConfig| {
            prepare_input(&db, account_id, EMAIL, raw.as_bytes(), config).unwrap()
        };

        let input = prepared(&account.id, &ThreatConfig::default());
        assert_eq!(input.receiver_domain.as_deref(), Some("example.org"));
        assert!(matches!(
            super::super::auth_results::sender_auth(&input),
            super::super::auth_results::SenderAuth::Pass { .. }
        ));

        let mut config = ThreatConfig::default();
        config
            .receiver_domains
            .insert(EMAIL.to_string(), "relay.example".to_string());
        let input = prepared(&account.id, &config);
        assert_eq!(input.receiver_domain.as_deref(), Some("relay.example"));

        // No account row: nothing to trust.
        let input = prepared("missing", &ThreatConfig::default());
        assert_eq!(input.receiver_domain, None);
    }

    #[test]
    fn reputation_lookups_become_domain_only_lookup_performed_events() {
        use super::super::reputation::{CACHE_FILE_NAME, ReputationAnalyzer, ReputationCache};
        let db = Database::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let log = LookupLog::default();
        let analyzers: Vec<Box<dyn Analyzer>> = vec![Box::new(ReputationAnalyzer::new(
            Box::new(ListedDns),
            None,
            ReputationCache::new(dir.path().join(CACHE_FILE_NAME)),
            log.clone(),
        ))];
        let raw = "Message-ID: <l1@x>\r\nFrom: IT <it@examp1e.org>\r\nTo: me@example.org\r\n\
                   Subject: s\r\n\r\nReset at https://portal.partner.example/reset?t=SECRET\r\n";
        let input = prepare_input(&db, ACCT, EMAIL, raw.as_bytes(), &ThreatConfig::default());
        let (verdict, scanned) = evaluate_with(input, &ThreatConfig::default(), &analyzers, &log);
        assert_eq!(verdict.signals[0].code, "domain_blocklisted");
        assert_eq!(verdict.signals[0].weight, 60);
        assert_eq!(scanned.lookups.len(), 2);
        assert!(log.lock().unwrap().is_empty(), "drained into the scan");

        let target = VerdictTarget {
            account_id: ACCT,
            folder: "INBOX",
            uid: 9,
            message_id: scanned.message_id.as_deref(),
            content_fingerprint: None,
            observed_message_ids: &[],
        };
        record_verdict(&db, &target, &verdict).unwrap();
        record_lookups(&db, &target, &scanned.lookups).unwrap();

        let mut stmt = db
            .conn()
            .prepare(
                "SELECT payload, uid, message_id, agent_id FROM events
                 WHERE event_type = 'lookup_performed' ORDER BY rowid",
            )
            .unwrap();
        let rows: Vec<(String, i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows.len(), 2);
        let payloads: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| serde_json::from_str(&r.0).unwrap())
            .collect();
        assert_eq!(
            payloads[0],
            json!({"provider": "spamhaus-dbl", "domain": "examp1e.org", "result": "listed:phish"})
        );
        assert_eq!(
            payloads[1],
            json!({"provider": "spamhaus-dbl", "domain": "partner.example", "result": "not_listed"})
        );
        for (payload, uid, mid, agent) in &rows {
            assert!(!payload.contains("SECRET") && !payload.contains("reset"));
            assert!(!payload.contains("portal.") && !payload.contains("it@"));
            assert_eq!(
                (*uid, mid.as_str(), agent.as_str()),
                (9, "l1@x", THREAT_AGENT_ID)
            );
        }
    }
}
