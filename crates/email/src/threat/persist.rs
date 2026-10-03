// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Where verdicts live and what they cause.
//!
//! - `message_scores` dimension `threat` (so `score_above threat N` rules
//!   match) and the `threat:*` tags;
//! - one pre-acked `threat_verdict` event per scan, payload = the verdict;
//! - quarantine: `tag` adds `threat:quarantined`; `move` also runs the shipped,
//!   editable rule `score_above threat 70 → move Envelope/Quarantine` through
//!   the unified executor as agent `envelope:threat`. Only `dangerous` mail
//!   is ever quarantined;
//! - the attachment gate every download/upload chokepoint calls.

use anyhow::{Context, Result, anyhow, bail};
use envelope_email_store::event_catalog::{LABEL_APPLIED, LOOKUP_PERFORMED, THREAT_VERDICT};
use envelope_email_store::models::{Event, Rule};
use envelope_email_store::{Database, canonical_message_id};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Analyzer, Level, LookupLog, LookupRecord, Quarantine, Signal, TAG_DANGEROUS,
    TAG_FALSE_POSITIVE, TAG_MALWARE, TAG_QUARANTINED, TAG_SUSPICIOUS, THREAT_DIMENSION,
    ThreatConfig, ThreatInput, ThreatVerdict, configured_analyzers, evaluate,
};
use crate::imap::{self, ImapClient};
use crate::rule_exec::{
    self, ActionAttribution, ActionSource, ExecDb, ImapRuleMailbox, MessageTarget, RuleMailbox,
    RuleRunReport, RunAccount,
};
use crate::rules::{Action, MatchExpr, MessageContext, StoredRuleAction};

pub const QUARANTINE_FOLDER: &str = "Envelope/Quarantine";
pub const QUARANTINE_RULE_NAME: &str = "Envelope threat quarantine";
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
    pub message_id: Option<String>,
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

/// Parse raw bytes and load the ledger: the part of a scan that needs the
/// database.
pub fn prepare_input(
    db: &Database,
    account_id: &str,
    account_address: &str,
    raw: &[u8],
) -> Result<ThreatInput, String> {
    let mut input = ThreatInput::from_raw(raw, account_address)?;
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
        message_id: header("message-id")
            .map(|m| canonical_message_id(&m).to_string())
            .filter(|m| !m.is_empty()),
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
    let (verdict, mut scanned) =
        evaluate_input(prepare_input(db, account_id, account_address, raw), config);
    scanned.content_fingerprint = Some(super::content_fingerprint(raw));
    (verdict, scanned)
}

/// Where a verdict is stored.
#[derive(Debug, Clone, Copy)]
pub struct VerdictTarget<'a> {
    pub account_id: &'a str,
    pub folder: &'a str,
    pub uid: u32,
    /// Canonical Message-ID. Without one, only the event is written (scores
    /// and tags are keyed by Message-ID).
    pub message_id: Option<&'a str>,
    /// [`super::content_fingerprint`] of the message's bytes, stored beside
    /// the verdict. `None` only where no complete bytes were read.
    pub content_fingerprint: Option<&'a str>,
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
}

/// True when Mark safe applies to the message with these bytes: its
/// Message-ID carries `threat:false_positive` and a Mark safe was recorded
/// for this fingerprint. A mark on other bytes with the same Message-ID, or
/// one made before fingerprints, does not apply; without a fingerprint
/// nothing is marked safe.
pub fn is_marked_safe(
    db: &Database,
    account_id: &str,
    message_id: &str,
    fingerprint: Option<&str>,
) -> Result<bool> {
    let Some(fingerprint) = fingerprint else {
        return Ok(false);
    };
    if !has_safe_tag(db, account_id, message_id)? {
        return Ok(false);
    }
    Ok(safe_marks(db, account_id, message_id)?
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

/// A Mark safe made before fingerprints on this folder/UID carries over to
/// the bytes there now. Legacy marks made anywhere else stay inert.
fn rebind_legacy_mark(
    db: &Database,
    target: &VerdictTarget<'_>,
    message_id: &str,
    fingerprint: &str,
) -> Result<()> {
    if !has_safe_tag(db, target.account_id, message_id)? {
        return Ok(());
    }
    let marks = safe_marks(db, target.account_id, message_id)?;
    let bound = marks
        .iter()
        .any(|(_, mark)| mark.content_fingerprint.as_deref() == Some(fingerprint));
    let legacy_here = marks.iter().any(|(event, mark)| {
        mark.content_fingerprint.is_none()
            && event.folder == target.folder
            && event.uid == Some(i64::from(target.uid))
    });
    if legacy_here && !bound {
        record_safe_mark(
            db,
            target,
            message_id,
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
    if let Some(mid) = target.message_id {
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
    record_verdict_event(db, target, verdict)
}

/// The `threat_verdict` event alone, without touching scores or tags.
fn record_verdict_event(
    db: &Database,
    target: &VerdictTarget<'_>,
    verdict: &ThreatVerdict,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let event = Event {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: target.account_id.to_string(),
        event_type: THREAT_VERDICT.to_string(),
        folder: target.folder.to_string(),
        uid: Some(i64::from(target.uid)),
        message_id: target.message_id.map(str::to_string),
        from_addr: None,
        subject: None,
        snippet: None,
        payload: Some(
            serde_json::to_string(&VerdictPayload {
                verdict: verdict.clone(),
                content_fingerprint: target.content_fingerprint.map(str::to_string),
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
    for lookup in lookups {
        let now = chrono::Utc::now().to_rfc3339();
        let event = Event {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: target.account_id.to_string(),
            event_type: LOOKUP_PERFORMED.to_string(),
            folder: target.folder.to_string(),
            uid: Some(i64::from(target.uid)),
            message_id: target.message_id.map(str::to_string),
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

/// The newest stored verdict for a message: by Message-ID when known, else
/// by folder/UID.
pub fn latest_verdict(
    db: &Database,
    account_id: &str,
    message_id: Option<&str>,
    folder: &str,
    uid: u32,
) -> Result<Option<ThreatVerdict>> {
    let event = match message_id {
        Some(mid) => db.latest_event_for_message(account_id, THREAT_VERDICT, mid)?,
        None => db.latest_event_for_uid(account_id, THREAT_VERDICT, folder, uid)?,
    };
    event
        .and_then(|e| e.payload)
        .map(|p| serde_json::from_str(&p).context("stored threat_verdict payload is not a verdict"))
        .transpose()
}

/// A stored verdict with the message identity its event recorded.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredVerdict {
    pub folder: String,
    pub uid: Option<i64>,
    pub message_id: Option<String>,
    pub recorded_at: String,
    pub verdict: ThreatVerdict,
    /// The fingerprint of the bytes the verdict judged; `None` for a verdict
    /// stored before fingerprints. Not part of any JSON output.
    #[serde(skip)]
    pub content_fingerprint: Option<String>,
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
/// verdict when its Message-ID matches and its fingerprint is equal or
/// missing (stored before fingerprints), else the newest verdict for the
/// Message-ID with the same fingerprint. A verdict on other bytes that share
/// the Message-ID never applies.
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
        && own
            .content_fingerprint
            .as_deref()
            .is_none_or(|fp| fp == fingerprint)
    {
        return Ok(Some(own));
    }
    let Some(mid) = message_id else {
        return Ok(None);
    };
    for event in db.events_for_message(account_id, THREAT_VERDICT, mid, FINGERPRINT_SEARCH_LIMIT)? {
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
    Ok(StoredVerdict {
        folder: event.folder,
        uid: event.uid,
        message_id: event.message_id,
        recorded_at: event.created_at,
        verdict: payload.verdict,
        content_fingerprint: payload.content_fingerprint,
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

/// The attachment gate. Refuses bytes that look like malware, or whose
/// message carries `threat:malware`, or whose message content (by
/// `fingerprint`) has a malware verdict on file. Only a Mark safe bound to
/// this fingerprint releases them, so an attachment fetched without one (part
/// by part, from an over-cap message) is never released by Mark safe.
pub fn attachment_block(
    db: &Database,
    account_id: &str,
    message_id: Option<&str>,
    fingerprint: Option<&str>,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<Option<AttachmentBlock>> {
    if let Some(mid) = message_id
        && is_marked_safe(db, account_id, mid, fingerprint)?
    {
        return Ok(None);
    }
    let signals = super::attachments::gate(filename, content_type, bytes);
    let reason = if !signals.is_empty() {
        Some(format!(
            "attachment looks like malware ({})",
            signals
                .iter()
                .map(|s| s.code.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    } else if let Some(mid) = message_id {
        message_malware_reason(db, account_id, mid, fingerprint)?
    } else {
        None
    };
    Ok(reason.map(|reason| AttachmentBlock {
        code: ATTACHMENT_BLOCKED,
        reason,
        signals,
    }))
}

/// Why the message makes every attachment refused, if it does: the
/// Message-ID's malware tag, or a malware verdict on these exact bytes. When
/// the verdict history is longer than the window read, a malware verdict
/// cannot be ruled out and the attachment is refused.
fn message_malware_reason(
    db: &Database,
    account_id: &str,
    message_id: &str,
    fingerprint: Option<&str>,
) -> Result<Option<String>> {
    if db
        .get_tags(account_id, message_id)?
        .iter()
        .any(|t| t.tag == TAG_MALWARE)
    {
        return Ok(Some("the message is tagged threat:malware".to_string()));
    }
    let Some(fingerprint) = fingerprint else {
        return Ok(None);
    };
    let events = db.events_for_message(
        account_id,
        THREAT_VERDICT,
        message_id,
        FINGERPRINT_SEARCH_LIMIT,
    )?;
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
    let message_id = parsed.message_id().map(canonical_message_id);
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
            message_id,
            Some(&fingerprint),
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
    let mid = target.message_id.ok_or_else(|| {
        anyhow!(
            "UID {} in {} has no Message-ID; cannot tag it",
            target.uid,
            target.folder
        )
    })?;
    let fingerprint = target.content_fingerprint.ok_or_else(|| {
        anyhow!(
            "{RESCAN_REQUIRED}: UID {} in {} has no content fingerprint to bind Mark safe to; \
             open or scan the message first",
            target.uid,
            target.folder
        )
    })?;
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

/// The `label_applied` event that binds a Mark safe to a fingerprint.
fn record_safe_mark(
    db: &Database,
    target: &VerdictTarget<'_>,
    message_id: &str,
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
        message_id: Some(message_id.to_string()),
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

/// Build the rule context for a scanned message from the stores.
fn rule_context(
    db: &Database,
    account_id: &str,
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
    Ok(MessageContext {
        from_addr: scanned.from_addr.clone(),
        to_addr: scanned.to_addr.clone(),
        subject: scanned.subject.clone(),
        tags,
        scores,
        contact_tags: db.get_contact_tags(account_id, &scanned.from_addr)?,
    })
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
    let Some(mid) = scanned.message_id.as_deref() else {
        return Ok(QuarantineOutcome::NotApplied);
    };
    let fingerprint = scanned.content_fingerprint.as_deref();
    let safe = db
        .with_db(|d| is_marked_safe(d, account.id, mid, fingerprint))
        .await?;
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
                rule_context(d, account.id, scanned)?,
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
        .with_db(|d| prepare_input(d, account.id, account.email, &raw))
        .await;
    // Opt-in analyzers block on clamd and DNS: run them off the async
    // workers and without holding the database.
    let owned = config.clone();
    let (verdict, mut scanned) = tokio::task::spawn_blocking(move || evaluate_input(input, &owned))
        .await
        .context("threat analyzers panicked")?;
    scanned.content_fingerprint = Some(super::content_fingerprint(&raw));
    db.with_db(|d| -> Result<()> {
        let target = VerdictTarget {
            account_id: account.id,
            folder,
            uid,
            message_id: scanned.message_id.as_deref(),
            content_fingerprint: scanned.content_fingerprint.as_deref(),
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

/// The canonical Message-ID of a raw message; `None` when it has none.
pub fn raw_message_id(raw: &[u8], account_address: &str) -> Option<String> {
    ThreatInput::from_raw(raw, account_address)
        .ok()
        .and_then(|i| {
            i.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("message-id"))
                .map(|(_, v)| canonical_message_id(v).to_string())
        })
        .filter(|m| !m.is_empty())
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
/// `raw` is `None` for a message read part by part (over the whole-message
/// fetch cap). There is nothing complete to scan, so this returns the stored
/// verdict, if any, and records nothing.
pub fn verdict_on_open(
    db: &Database,
    account_id: &str,
    account_address: &str,
    folder: &str,
    uid: u32,
    raw: Option<&[u8]>,
    config: &ThreatConfig,
) -> Result<Option<ThreatVerdict>> {
    let Some(raw) = raw else {
        return latest_verdict(db, account_id, None, folder, uid);
    };
    let message_id = raw_message_id(raw, account_address);
    let fingerprint = super::content_fingerprint(raw);
    let here = VerdictTarget {
        account_id,
        folder,
        uid,
        message_id: message_id.as_deref(),
        content_fingerprint: Some(&fingerprint),
    };
    if let Some(mid) = here.message_id {
        rebind_legacy_mark(db, &here, mid, &fingerprint)?;
    }
    let matched = matching_verdict(
        db,
        account_id,
        folder,
        uid,
        message_id.as_deref(),
        &fingerprint,
    )?;
    if !config.enabled || !config.on_read {
        return Ok(matched.map(|m| m.verdict));
    }
    if let Some(m) = matched
        && m.content_fingerprint.is_some()
        && !needs_scan(Some(&m.verdict))
    {
        if (m.folder.as_str(), m.uid) != (folder, Some(i64::from(uid))) {
            record_verdict_event(db, &here, &m.verdict)?;
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
    };
    scan_one(&mut mbox, db, account, folder, uid, config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::threat::{ENGINE_VERSION, content_fingerprint};

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
            },
            &verdict,
        )
        .unwrap();
        assert_eq!(tags(&db, "p1@x"), vec![TAG_DANGEROUS, TAG_MALWARE]);
        let score = db.get_scores(ACCT, "p1@x").unwrap();
        assert_eq!(score[0].dimension, THREAT_DIMENSION);
        assert_eq!(score[0].value, f64::from(verdict.score));

        let stored = latest_verdict(&db, ACCT, Some("p1@x"), "INBOX", 7)
            .unwrap()
            .unwrap();
        assert_eq!(stored, verdict);
        let event = db
            .latest_event_for_message(ACCT, THREAT_VERDICT, "p1@x")
            .unwrap()
            .unwrap();
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
        };
        let (bad, _) = scan_raw(&db, ACCT, EMAIL, &phish("m@x"), &ThreatConfig::default());
        record_verdict(&db, &target, &bad).unwrap();
        record_verdict(&db, &target, &ThreatVerdict::unavailable("clamd down")).unwrap();
        assert!(tags(&db, "m@x").is_empty());
        assert!(db.get_scores(ACCT, "m@x").unwrap().is_empty());
        assert_eq!(
            latest_verdict(&db, ACCT, Some("m@x"), "INBOX", 1)
                .unwrap()
                .unwrap()
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
    fn needs_scan_on_missing_or_old_engine() {
        assert!(needs_scan(None));
        let mut v = ThreatVerdict::unavailable("x");
        assert!(!needs_scan(Some(&v)));
        v.engine_version = "rshield-0".to_string();
        assert!(needs_scan(Some(&v)));
        assert_eq!(ENGINE_VERSION, "rshield-1");
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
            .latest_event_for_message(ACCT, LABEL_APPLIED, "a@x")
            .unwrap()
            .unwrap();
        assert!(label.payload.unwrap().contains(TAG_FALSE_POSITIVE));
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

    /// A clean message that reuses a malware message's Message-ID clears the
    /// Message-ID's malware tag, but the original's attachments stay blocked:
    /// its own verdict, found by fingerprint, is still malware.
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
        let scanned = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 1, Some(original), &config)
            .unwrap()
            .unwrap();
        assert!(scanned.is_malware());
        let resend = verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            2,
            Some(&ordinary("orig@x")),
            &config,
        )
        .unwrap()
        .unwrap();
        assert_eq!(resend.level, Level::Clean);
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

    /// The gate reads a bounded window of a Message-ID's verdicts. When the
    /// window is full and holds no malware verdict for these bytes, it cannot
    /// rule one out, so it refuses.
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
        let target = |uid, fp| VerdictTarget {
            account_id: ACCT,
            folder: "INBOX",
            uid,
            message_id: Some("long@x"),
            content_fingerprint: Some(fp),
        };
        record_verdict_event(&db, &target(1, "v1:orig"), &malware).unwrap();
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
            record_verdict_event(&db, &target(uid, "v1:other"), &clean).unwrap();
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
        assert!(
            verdict_on_open(&db, ACCT, EMAIL, "INBOX", 2379, None, &config)
                .unwrap()
                .is_none()
        );
        assert!(
            latest_verdict(&db, ACCT, None, "INBOX", 2379)
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
        };
        let stored = ThreatVerdict::unavailable("clamd down");
        record_verdict(&db, &target, &stored).unwrap();
        assert_eq!(
            verdict_on_open(&db, ACCT, EMAIL, "INBOX", 2379, None, &config).unwrap(),
            Some(stored)
        );
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
            },
            &clean,
        )
        .unwrap();
        let off = ThreatConfig {
            on_read: false,
            ..ThreatConfig::default()
        };
        assert_eq!(
            verdict_on_open(&db, ACCT, EMAIL, "INBOX", 4, Some(&raw), &off).unwrap(),
            Some(clean)
        );

        let config = ThreatConfig::default();
        let opened = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 4, Some(&raw), &config)
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
            Some(&ordinary("dup@x")),
            &config,
        )
        .unwrap()
        .unwrap();
        assert_eq!(first.level, Level::Clean);

        let second = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 2, Some(&phish("dup@x")), &config)
            .unwrap()
            .unwrap();
        assert_eq!(second.level, Level::Dangerous);
        assert!(second.is_malware());
        assert_eq!(tags(&db, "dup@x"), vec![TAG_DANGEROUS, TAG_MALWARE]);

        let again = verdict_on_open(
            &db,
            ACCT,
            EMAIL,
            "INBOX",
            1,
            Some(&ordinary("dup@x")),
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
        let verdict = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 1, Some(&raw), &config)
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
            },
            "cli",
            None,
        )
        .unwrap();

        let moved = verdict_on_open(&db, ACCT, EMAIL, "Archive", 9, Some(&raw), &config)
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
        verdict_on_open(&db, ACCT, EMAIL, "INBOX", 1, Some(&original), &config).unwrap();
        mark_safe(
            &db,
            &VerdictTarget {
                account_id: ACCT,
                folder: "INBOX",
                uid: 1,
                message_id: Some("reuse@x"),
                content_fingerprint: Some(&original_fp),
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
        assert_eq!(
            db.get_scores(ACCT, "reuse@x").unwrap()[0].value,
            f64::from(entry.score)
        );
        assert_eq!(
            tags(&db, "reuse@x"),
            vec![
                TAG_DANGEROUS,
                TAG_FALSE_POSITIVE,
                TAG_MALWARE,
                TAG_QUARANTINED
            ]
        );
        let opened = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 2, Some(&resend), &config)
            .unwrap()
            .unwrap();
        assert_eq!(opened.level, Level::Dangerous);
        let resend_fp = content_fingerprint(&resend);
        assert!(!is_marked_safe(&db, ACCT, "reuse@x", Some(&resend_fp)).unwrap());
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

        let opened = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 5, Some(&raw), &config)
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

        let opened = verdict_on_open(&db, ACCT, EMAIL, "INBOX", 3, Some(&raw), &config)
            .unwrap()
            .unwrap();
        assert_eq!(opened.level, Level::Dangerous, "the engine's view is kept");
        assert!(is_marked_safe(&db, ACCT, "slot@x", Some(&fp)).unwrap());
        assert_eq!(tags(&db, "slot@x"), vec![TAG_FALSE_POSITIVE]);
        assert_eq!(db.get_scores(ACCT, "slot@x").unwrap()[0].value, 0.0);
        let other = content_fingerprint(&ordinary("slot@x"));
        assert!(!is_marked_safe(&db, ACCT, "slot@x", Some(&other)).unwrap());
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
        let input = prepare_input(&db, ACCT, EMAIL, raw.as_bytes());
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
