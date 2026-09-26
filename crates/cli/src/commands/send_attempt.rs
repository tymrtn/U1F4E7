// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! One send operation, from request to receipt, for every CLI/MCP surface.
//!
//! - [`send_now`]: `send --send-now` and MCP `send`/`reply` with `send_now`.
//!   The request is written as a durable intent row before any network work,
//!   so a rerun of the same request finds it instead of sending again.
//! - [`queue_request`]: the queued forms of the same requests.
//! - [`claim_row`] + [`transmit_claimed`]: one attempt of a row, shared with
//!   `draft send --send-now` and MCP `send_draft`.
//!
//! An attempt commits `transmitting` after the server answers DATA and
//! before the first body byte. Until then any failure releases the row for a
//! later attempt; after it, only the server's own refusal does. Anything else
//! parks the row `delivery_uncertain`, and nothing re-sends it.

use anyhow::{Context, Result, anyhow};
use envelope_email_store::models::{AccountWithCredentials, Draft, DraftStatus};
use envelope_email_store::send_attempts::display_status;
use envelope_email_store::{
    AttemptClaim, AttemptLock, AttemptStart, ClaimMode, Database, IntentKey, IntentLookup,
    NewSendIntent, QueueContext, ReleaseBasis,
};
use envelope_email_transport::outbound::{GovernorOutcome, SendSurface};
use envelope_email_transport::sent_proof::SentCopyResult;
use envelope_email_transport::smtp::{Attachment, build_message, generate_message_id};
use envelope_email_transport::smtp_submit::{
    Deadlines, SmtpConnect, SubmitFailure, open_submission,
};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::attachments::{attachment_summaries, decode_attachments};
use super::governor_gate::{account_domain, gate_and_record_with_agent, governor_request};
use super::ui;

/// A send that ended without a confirmed acceptance, carrying the
/// structured result the caller prints (`status`, `retryable`, `error`).
#[derive(Debug)]
pub(crate) struct SendNotConfirmed {
    pub body: Value,
}

impl std::fmt::Display for SendNotConfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = self.body["error"]["reason"]
            .as_str()
            .or_else(|| self.body["status"].as_str())
            .unwrap_or("send not confirmed");
        match self.body["draft_id"].as_str() {
            Some(id) if !reason.contains(id) => write!(f, "{reason} (draft {id})"),
            Some(_) => f.write_str(reason),
            None => f.write_str(reason),
        }
    }
}

impl std::error::Error for SendNotConfirmed {}

/// The Governor gate refused the attempt. The row was released; its
/// `Display` is the gate's canonical `{status, error}` JSON.
#[derive(Debug)]
pub(crate) struct GovernorRefused {
    pub outcome: GovernorOutcome,
}

impl std::fmt::Display for GovernorRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.outcome.response_json())
    }
}

impl std::error::Error for GovernorRefused {}

/// One requested message.
pub(crate) struct SendRequest<'a> {
    pub surface: SendSurface,
    /// Receipt label: `cli_send`, `mcp_send`, `mcp_reply`.
    pub label: &'static str,
    /// Who asked: `local` for the CLI operator, `agent:<id>` for an agent.
    pub principal: String,
    pub agent_id: Option<&'a str>,
    pub idempotency_key: Option<&'a str>,
    pub to: &'a str,
    pub cc: Option<&'a str>,
    pub bcc: Option<&'a str>,
    pub reply_to: Option<&'a str>,
    pub subject: &'a str,
    pub text: Option<&'a str>,
    pub html: Option<&'a str>,
    pub from: Option<&'a str>,
    pub in_reply_to: Option<&'a str>,
    pub references: &'a [String],
    /// Snapshot entries with `data_base64`: read once, stored with the
    /// intent, and transmitted from the stored bytes.
    pub attachments: &'a [Value],
    pub declared: &'a [String],
    /// Extra metadata for the row (`draft_kind`, `source`).
    pub metadata: Value,
    pub created_by: &'static str,
}

impl SendRequest<'_> {
    fn intent<'s>(&'s self, account_id: &'s str) -> NewSendIntent<'s> {
        let mut metadata = match self.metadata.clone() {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        metadata.insert("agent_body_text".into(), json!(self.text));
        metadata.insert("agent_body_html".into(), json!(self.html));
        if let Some(from) = self.from {
            metadata.insert("from".into(), json!(from));
        }
        if self.in_reply_to.is_some() || !self.references.is_empty() {
            metadata.insert("in_reply_to".into(), json!(self.in_reply_to));
            metadata.insert("references".into(), json!(self.references));
        }
        NewSendIntent {
            account_id,
            principal: &self.principal,
            agent_id: self.agent_id,
            surface: self.label,
            key: match self.idempotency_key {
                Some(key) => IntentKey::Explicit(key),
                None => IntentKey::Fingerprint,
            },
            created_by: self.created_by,
            to: self.to,
            cc: self.cc,
            bcc: self.bcc,
            reply_to: self.reply_to,
            subject: self.subject,
            text: self.text,
            html: self.html,
            in_reply_to: self.in_reply_to,
            attachments: self.attachments,
            metadata: Value::Object(metadata),
        }
    }

    fn attempt_surface(&self) -> AttemptSurface<'_> {
        AttemptSurface {
            governor: self.surface,
            declared: self.declared,
            agent_id: self.agent_id,
        }
    }
}

/// Whether [`transmit_claimed`] still has to run the Governor gate.
pub(crate) enum Gate {
    Run,
    /// The caller gated this exact content before claiming the row.
    Passed(GovernorOutcome),
}

/// Who is transmitting, for the Governor gate and receipts.
pub(crate) struct AttemptSurface<'a> {
    pub governor: SendSurface,
    pub declared: &'a [String],
    pub agent_id: Option<&'a str>,
}

/// The message as it will be transmitted.
pub(crate) struct Outgoing {
    pub to: String,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    pub cc: Option<String>,
    pub bcc: Option<String>,
    pub reply_to: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// The `From:` header value.
    pub from: String,
    pub attachments: Vec<Attachment>,
}

impl Outgoing {
    /// Everything from the claimed row: the stored content, threading,
    /// sender identity and attachment bytes.
    pub(crate) fn of_draft(draft: &Draft, creds: &AccountWithCredentials) -> Result<Self> {
        let (in_reply_to, references, _) = super::drafts::threading_for_draft(draft);
        let references = envelope_email_transport::reply::ensure_references_chain(
            &references,
            in_reply_to.as_deref(),
        );
        Ok(Self {
            to: draft.to_addr.clone(),
            subject: draft.subject.clone().unwrap_or_default(),
            text: draft.text_content.clone(),
            html: draft.html_content.clone(),
            cc: draft.cc_addr.clone(),
            bcc: draft.bcc_addr.clone(),
            reply_to: draft.reply_to.clone(),
            in_reply_to,
            references,
            from: super::drafts::from_header_for_draft(draft.metadata.as_ref(), creds),
            attachments: decode_attachments(&draft.attachments)
                .context("failed to decode draft attachments")?,
        })
    }
}

/// A transmission the server accepted.
pub(crate) struct SentAttempt {
    pub draft_id: String,
    pub message_id: String,
    pub attribution: Option<Value>,
    pub copy: SentCopyResult,
    pub imap_draft_deleted: bool,
    /// Non-empty when the acceptance could not be recorded as `sent`.
    pub warnings: Vec<Value>,
}

/// A claimed row plus the owner lock that marks this process as alive. An
/// in-memory database has no lock: no other process can see its rows.
pub(crate) struct HeldClaim {
    pub claim: AttemptClaim,
    pub lock: Option<AttemptLock>,
}

/// What an existing row means for a new request.
enum Answer {
    /// A `draft` row this request may claim or queue.
    Resume(Draft),
    /// Already done or already in the outbox: report it, send nothing.
    Replay(Draft),
    /// Not sendable by this request: report why, send nothing.
    Refuse(Value),
}

fn answer_existing(draft: Draft) -> Answer {
    match draft.status {
        DraftStatus::Draft => Answer::Resume(draft),
        DraftStatus::Sent => Answer::Replay(draft),
        _ => Answer::Refuse(state_report(&draft)),
    }
}

/// The attempt's Message-ID for a row, or the stored one once sent.
fn attempt_message_id(draft: &Draft) -> Option<String> {
    if draft.status == DraftStatus::Sent {
        return draft.message_id.clone();
    }
    draft
        .metadata
        .as_ref()
        .and_then(|m| m.get("send_attempt"))
        .and_then(|a| a.get("message_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Why a row cannot be sent by this request, and whether rerunning helps.
pub(crate) fn state_report(draft: &Draft) -> Value {
    let (code, retryable, reason) = match draft.status {
        DraftStatus::DeliveryUncertain => (
            "delivery_uncertain",
            false,
            "An earlier attempt of this message may have been delivered, so Envelope will not \
             send it again. Check the recipient or the Sent folder; to send a new copy, discard \
             this draft and send again.",
        ),
        DraftStatus::Sending => (
            "send_in_progress",
            true,
            "Another process is sending this message now. Rerun the same command to get its \
             outcome.",
        ),
        DraftStatus::Syncing => (
            "sync_in_progress",
            true,
            "This draft is being synced to the mailbox. Rerun the same command shortly.",
        ),
        DraftStatus::PendingReview | DraftStatus::Blocked => (
            "awaiting_review",
            false,
            "This message is parked for a person to review before it can be sent.",
        ),
        DraftStatus::Discarded => (
            "discarded",
            false,
            "This message was discarded. Use a new idempotency key to send it again.",
        ),
        DraftStatus::Draft | DraftStatus::Sent => (
            "draft_changed",
            true,
            "The draft changed while this send was starting. Rerun the command.",
        ),
    };
    json!({
        "status": display_status(draft),
        "draft_id": draft.id,
        "message_id": attempt_message_id(draft),
        "retryable": retryable,
        "idempotent_replay": true,
        "error": {"code": code, "reason": reason},
        "ui": ui::draft_ui(&draft.account_id, &draft.id),
    })
}

fn key_conflict(draft: &Draft) -> anyhow::Error {
    SendNotConfirmed {
        body: json!({
            "status": "idempotency_key_conflict",
            "draft_id": draft.id,
            "retryable": false,
            "error": {
                "code": "idempotency_key_conflict",
                "reason": "This idempotency key already names a different message (or one \
                           edited since). Nothing was sent. Use a new key for new content.",
            },
            "ui": ui::draft_ui(&draft.account_id, &draft.id),
        }),
    }
    .into()
}

/// The recorded outcome of a `sent` row, in the shape of a fresh send.
pub(crate) fn sent_replay(draft: &Draft) -> Value {
    let copy = draft
        .metadata
        .as_ref()
        .and_then(|m| m.get("sent_copy"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "status": "sent",
        "sent": true,
        "draft_id": draft.id,
        "to": draft.to_addr,
        "subject": draft.subject,
        "message_id": draft.message_id,
        "sent_folder": copy.get("folder").cloned().unwrap_or(Value::Null),
        "sent_uid": copy.get("uid").cloned().unwrap_or(Value::Null),
        "sent_mail_appended": copy.get("copy_source").and_then(Value::as_str) == Some("client_appended"),
        "sent_mail": copy,
        "attribution": Value::Null,
        "attachments": attachment_summaries(&draft.attachments),
        "idempotent_replay": true,
        "ui": ui::draft_ui(&draft.account_id, &draft.id),
    })
}

/// Take the owner lock for `start` and claim the row for one attempt.
/// `Ok(None)` means another actor holds or changed the row.
pub(crate) fn claim_row(
    db: &Database,
    draft: &Draft,
    start: &AttemptStart<'_>,
) -> Result<Option<HeldClaim>> {
    let lock = db
        .send_lock_dir()
        .map(|dir| AttemptLock::acquire(&dir, &start.attempt_id))
        .transpose()?;
    Ok(db
        .claim_send_attempt(&draft.id, draft.revision, ClaimMode::Immediate, start)
        .context("failed to claim draft for sending")?
        .map(|claim| HeldClaim { claim, lock }))
}

/// Send `req` now, at most once per intent.
///
/// A rerun of the same request (same explicit key, or the same payload with
/// no key) answers from the recorded intent: a `sent` intent is replayed, an
/// unresolved one is reported, and only a `draft` intent (never started, or
/// released after a failure that sent nothing) is attempted again.
pub(crate) async fn send_now<C: SmtpConnect>(
    db: &Database,
    creds: &AccountWithCredentials,
    req: &SendRequest<'_>,
    connector: &C,
) -> Result<Value> {
    db.reconcile_stale_sending_now()
        .context("could not recover stale sends before sending")?;
    let spec = req.intent(&creds.account.id);

    // Answer from an earlier request before anything else: a replay makes no
    // Governor call and sends nothing.
    if let Some(found) = db.lookup_send_intent(&spec)?
        && let Next::Replayed(body) = answer_or_resume(db, req, found)?
    {
        return Ok(body);
    }

    // A new transmission. Gate it before recording anything, so a refused
    // request leaves no row behind.
    let gov = gate_request(db, creds, req)?;
    let draft = match answer_or_resume(db, req, db.find_or_create_send_intent(&spec)?)? {
        Next::Transmit(draft) => draft,
        Next::Replayed(body) => return Ok(body),
    };

    let message_id = format!("<{}>", generate_message_id(creds));
    let start = AttemptStart::new(&message_id, req.label, req.agent_id);
    let Some(held) = claim_row(db, &draft, &start)? else {
        return claim_lost(db, req, &draft.id);
    };
    let outgoing = Outgoing::of_draft(&held.claim.draft, creds)?;
    let sent = transmit_claimed(
        db,
        creds,
        held,
        &outgoing,
        &req.attempt_surface(),
        Gate::Passed(gov),
        connector,
    )
    .await?;
    Ok(sent_json(&sent, &outgoing, &creds.account.id))
}

/// The answer when another request for the same intent claimed the row
/// between this request's lookup and its claim.
fn claim_lost(db: &Database, req: &SendRequest<'_>, draft_id: &str) -> Result<Value> {
    let current = db
        .get_draft(draft_id)?
        .ok_or_else(|| anyhow!("draft vanished: {draft_id}"))?;
    match answer_or_resume(db, req, IntentLookup::Existing(current.clone()))? {
        Next::Replayed(body) => Ok(body),
        // Released again by the other request: report it rather than race it.
        Next::Transmit(_) => Err(SendNotConfirmed {
            body: state_report(&current),
        }
        .into()),
    }
}

enum Next {
    Transmit(Draft),
    /// Already sent by an earlier identical request: its recorded result.
    Replayed(Value),
}

/// Continue with `found` if it is a row this request may transmit, or end
/// the request with its recorded answer.
fn answer_or_resume(db: &Database, req: &SendRequest<'_>, found: IntentLookup) -> Result<Next> {
    match found {
        IntentLookup::Created(draft) => Ok(Next::Transmit(draft)),
        IntentLookup::KeyConflict(draft) => Err(key_conflict(&draft)),
        IntentLookup::Existing(draft) => match answer_existing(draft) {
            Answer::Resume(draft) => Ok(Next::Transmit(draft)),
            Answer::Replay(draft) => {
                db.record_send_replay(&draft.id, req.agent_id, req.label)?;
                Ok(Next::Replayed(sent_replay(&draft)))
            }
            Answer::Refuse(body) => Err(SendNotConfirmed { body }.into()),
        },
    }
}

/// The Governor gate for a request's own content, before any row exists.
fn gate_request(
    db: &Database,
    creds: &AccountWithCredentials,
    req: &SendRequest<'_>,
) -> Result<GovernorOutcome> {
    let attachments = decode_attachments(req.attachments)?;
    let gov_req = governor_request(
        db,
        &creds.account.id,
        account_domain(&creds.account.username),
        req.subject,
        req.to,
        req.cc,
        req.bcc,
        req.surface,
        None,
        &attachments,
        req.in_reply_to,
        req.text,
        req.html,
        req.declared,
    );
    let gov = gate_and_record_with_agent(db, &creds.account.id, &gov_req, req.agent_id)?;
    if gov.allowed {
        Ok(gov)
    } else {
        Err(GovernorRefused { outcome: gov }.into())
    }
}

/// The success result of a fresh send.
pub(crate) fn sent_json(sent: &SentAttempt, outgoing: &Outgoing, account_id: &str) -> Value {
    use super::drafts::{SentMailProofUi, sent_copy_convenience_objects, sent_mail_proof_json};
    let proof = &sent.copy.proof;
    let (provider_sent_copy, client_appended_copy) =
        sent_copy_convenience_objects(account_id, proof);
    let mut body = json!({
        "status": "sent",
        "sent": true,
        "draft_id": sent.draft_id,
        "to": outgoing.to,
        "subject": outgoing.subject,
        "message_id": sent.message_id,
        "imap_draft_deleted": sent.imap_draft_deleted,
        "sent_mail_appended": sent.copy.sent_mail_appended,
        "sent_mail_append_skipped_reason": sent.copy.sent_mail_append_skipped_reason,
        "sent_folder": proof.folder.clone(),
        "sent_uid": proof.uid,
        "sent_message_url": proof.message_url(account_id),
        "sent_mail": sent_mail_proof_json(account_id, proof),
        "provider_sent_copy": provider_sent_copy,
        "client_appended_copy": client_appended_copy,
        "attribution": sent.attribution,
        "attachments": outgoing.attachments.iter().map(|a| json!({
            "filename": a.filename,
            "content_type": a.content_type,
            "size": a.data.len(),
        })).collect::<Vec<_>>(),
        "idempotent_replay": false,
        "ui": proof.ui(account_id),
        "draft_ui": ui::draft_ui(account_id, &sent.draft_id),
    });
    if !sent.warnings.is_empty() {
        body["warnings"] = json!(sent.warnings);
    }
    body
}

/// What a queued request resolved to.
pub(crate) enum Queued {
    /// The intent is in the outbox; `replay` when an earlier request put it there.
    Queued { draft: Box<Draft>, replay: bool },
    /// The intent was already sent.
    Sent(Value),
}

/// Record `req` as an intent and put it in the outbox for `send_after`,
/// or answer from an earlier identical request.
pub(crate) fn queue_request(
    db: &Database,
    account_id: &str,
    req: &SendRequest<'_>,
    send_after: &str,
    cooldown_seconds: Option<i64>,
) -> Result<Queued> {
    let (draft, replay) = match db.find_or_create_send_intent(&req.intent(account_id))? {
        IntentLookup::Created(draft) => (draft, false),
        IntentLookup::KeyConflict(draft) => return Err(key_conflict(&draft)),
        IntentLookup::Existing(draft) => match answer_existing(draft) {
            Answer::Resume(draft) if draft.send_after.is_some() => {
                db.record_send_replay(&draft.id, req.agent_id, req.label)?;
                return Ok(Queued::Queued {
                    draft: Box::new(draft),
                    replay: true,
                });
            }
            Answer::Resume(draft) => (draft, true),
            Answer::Replay(draft) => {
                db.record_send_replay(&draft.id, req.agent_id, req.label)?;
                return Ok(Queued::Sent(sent_replay(&draft)));
            }
            Answer::Refuse(body) => return Err(SendNotConfirmed { body }.into()),
        },
    };
    super::drafts::queue_bot_draft_for_send(
        db,
        &draft.id,
        draft.revision,
        send_after,
        req.declared,
        &QueueContext {
            surface: req.label,
            agent_id: req.agent_id,
            cooldown_seconds,
        },
    )?;
    let draft = db
        .get_draft(&draft.id)?
        .ok_or_else(|| anyhow!("queued draft vanished: {}", draft.id))?;
    Ok(Queued::Queued {
        draft: Box::new(draft),
        replay,
    })
}

/// Releases a still-`claimed` attempt when the send stops before its body
/// starts: any early return, error, panic, or a dropped future. Once the
/// attempt is `transmitting` the store refuses the release, so the guard can
/// never return a possibly-accepted message to the outbox.
struct ClaimGuard<'a> {
    db: &'a Database,
    draft_id: String,
    token: String,
    armed: bool,
}

impl<'a> ClaimGuard<'a> {
    fn new(db: &'a Database, draft_id: &str, token: &str) -> Self {
        Self {
            db,
            draft_id: draft_id.to_string(),
            token: token.to_string(),
            armed: true,
        }
    }

    fn release(&mut self, reason: &str, evidence: Option<Value>) -> Result<bool> {
        self.armed = false;
        Ok(self.db.release_attempt(
            &self.draft_id,
            &self.token,
            DraftStatus::Draft,
            ReleaseBasis::NotStarted,
            reason,
            evidence,
            None,
        )?)
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match self.db.release_attempt(
            &self.draft_id,
            &self.token,
            DraftStatus::Draft,
            ReleaseBasis::NotStarted,
            "stopped_before_body",
            None,
            None,
        ) {
            Ok(_) => {}
            Err(e) => warn!(
                "draft {}: could not release the send claim: {e}. It stays claimed until \
                 recovery releases it after this process exits.",
                self.draft_id
            ),
        }
    }
}

fn not_sent(draft: &Draft, failure: &SubmitFailure) -> anyhow::Error {
    let (reply_code, permanent) = match failure {
        SubmitFailure::NotSubmitted {
            reply_code,
            permanent,
            ..
        } => (*reply_code, *permanent),
        SubmitFailure::Uncertain { .. } => (None, false),
    };
    SendNotConfirmed {
        body: json!({
            "status": "not_sent",
            "draft_id": draft.id,
            "retryable": failure.retryable(),
            "error": {
                "code": if reply_code.is_some() { "smtp_refused" } else { "smtp_unavailable" },
                "stage": failure.stage().as_str(),
                "reply_code": reply_code,
                "permanent": permanent,
                "reason": format!(
                    "{failure}. Nothing was sent; the message is kept as draft {}.",
                    draft.id
                ),
            },
            "ui": ui::draft_ui(&draft.account_id, &draft.id),
        }),
    }
    .into()
}

/// One transmission attempt of a claimed row: Governor gate, SMTP, and the
/// durable outcome, then (after acceptance) provider Drafts cleanup and the
/// Sent copy.
pub(crate) async fn transmit_claimed<C: SmtpConnect>(
    db: &Database,
    creds: &AccountWithCredentials,
    held: HeldClaim,
    outgoing: &Outgoing,
    surface: &AttemptSurface<'_>,
    gate: Gate,
    connector: &C,
) -> Result<SentAttempt> {
    let HeldClaim { claim, lock } = held;
    let draft = &claim.draft;
    let mut guard = ClaimGuard::new(db, &draft.id, &claim.token);

    // ── Governor gate (fail-closed before any real SMTP) ──
    let gov = match gate {
        Gate::Passed(gov) => gov,
        Gate::Run => gate_claimed(db, creds, draft, outgoing, surface, &mut guard)?,
    };

    let references = (!outgoing.references.is_empty()).then_some(outgoing.references.as_slice());
    let bare_id = claim.message_id.trim_matches(|c| c == '<' || c == '>');
    let (message, header_id) = build_message(
        creds,
        bare_id,
        &outgoing.to,
        &outgoing.subject,
        outgoing.text.as_deref(),
        outgoing.html.as_deref(),
        Some(&outgoing.from),
        outgoing.cc.as_deref(),
        outgoing.bcc.as_deref(),
        false, // real send: drop Bcc from the wire
        outgoing.reply_to.as_deref(),
        outgoing.in_reply_to.as_deref(),
        references,
        &outgoing.attachments,
    )?;
    let body = message.formatted();

    info!(
        "sending draft {} via {}:{} ({} attachment(s))",
        draft.id,
        creds.account.smtp_host,
        creds.account.smtp_port,
        outgoing.attachments.len()
    );
    let open = match open_submission(
        connector,
        message.envelope(),
        body.is_ascii(),
        Deadlines::default(),
    )
    .await
    {
        Ok(open) => open,
        Err(failure) => {
            guard.release("smtp_not_submitted", Some(failure.evidence()))?;
            return Err(not_sent(draft, &failure));
        }
    };

    // The durable point of no return: committed before the first body byte.
    match db.begin_transmitting(&draft.id, &claim.token) {
        Ok(true) => guard.disarm(),
        Ok(false) => {
            open.abort().await;
            let current = db
                .get_draft(&draft.id)?
                .ok_or_else(|| anyhow!("draft vanished: {}", draft.id))?;
            return Err(SendNotConfirmed {
                body: state_report(&current),
            }
            .into());
        }
        Err(e) => {
            open.abort().await;
            return Err(anyhow!(e).context(
                "audit_unavailable: could not record the transmission before sending; \
                 nothing was sent",
            ));
        }
    }

    let accepted = match open.transmit(&body).await {
        Ok(accepted) => accepted,
        Err(failure @ SubmitFailure::NotSubmitted { .. }) => {
            db.release_attempt(
                &draft.id,
                &claim.token,
                DraftStatus::Draft,
                ReleaseBasis::Refused,
                "smtp_refused",
                Some(failure.evidence()),
                None,
            )?;
            return Err(not_sent(draft, &failure));
        }
        Err(failure) => {
            let parked = db
                .park_attempt_uncertain(
                    &draft.id,
                    &claim.token,
                    "smtp_outcome_unknown",
                    Some(failure.evidence()),
                )
                .unwrap_or_else(|e| {
                    warn!("draft {}: could not park the attempt: {e}", draft.id);
                    false
                });
            return Err(SendNotConfirmed {
                body: json!({
                    "status": "delivery_uncertain",
                    "draft_id": draft.id,
                    "message_id": claim.message_id,
                    "retryable": false,
                    "recorded": parked,
                    "error": {
                        "code": "delivery_uncertain",
                        "stage": failure.stage().as_str(),
                        "reason": format!(
                            "{failure}. The server may have accepted the message, so Envelope \
                             will not send it again. Check the recipient or the Sent folder \
                             for Message-ID {}.",
                            claim.message_id
                        ),
                    },
                    "ui": ui::draft_ui(&draft.account_id, &draft.id),
                }),
            }
            .into());
        }
    };

    // Accepted. Record it before QUIT; a hung QUIT cannot change the outcome.
    let reply = accepted.reply.clone();
    let recorded = db.finish_attempt_sent(
        &draft.id,
        &claim.token,
        &header_id,
        json!({"kind": "smtp_acceptance", "reply": reply}),
    );
    accepted.close().await;
    let mut warnings = Vec::new();
    if let Err(e) = &recorded {
        let parked = db
            .park_attempt_uncertain(
                &draft.id,
                &claim.token,
                "sent_state_unrecorded",
                Some(json!({"kind": "smtp_acceptance", "reply": reply})),
            )
            .unwrap_or(false);
        warn!(
            "draft {} was accepted (message_id={header_id}) but its sent state could not be \
             recorded: {e}. Parked delivery_uncertain={parked}; it will never be re-sent.",
            draft.id
        );
        warnings.push(json!({
            "code": "audit_write_failed",
            "detail": format!("the server accepted the message but Envelope could not record it: {e}"),
            "record_status": if parked { "delivery_uncertain" } else { "sending" },
        }));
    }
    drop(lock);

    // ── Provider draft cleanup: exact + unique, only after durable sent state ──
    let mut imap_draft_deleted = false;
    if recorded.is_ok() && !creds.account.imap_host.trim().is_empty() {
        imap_draft_deleted = super::drafts::cleanup_provider_draft_copy(db, creds, draft).await;
    }

    // ── Sent-folder copy (pre-lookup before any client append) ──
    let provider_type = db.get_provider_type(&draft.account_id).ok().flatten();
    let copy = super::drafts::resolve_sent_copy_after_send(
        db,
        creds,
        provider_type.as_deref(),
        &outgoing.from,
        &outgoing.to,
        &outgoing.subject,
        outgoing.text.as_deref(),
        outgoing.html.as_deref(),
        outgoing.cc.as_deref(),
        outgoing.bcc.as_deref(),
        outgoing.reply_to.as_deref(),
        outgoing.in_reply_to.as_deref(),
        &outgoing.references,
        &header_id,
        &outgoing.attachments,
    )
    .await;
    if recorded.is_ok() {
        match db.record_sent_copy_proof(
            &draft.id,
            copy.proof.folder.as_deref(),
            copy.proof.uid,
            copy.proof.lookup_status,
            copy.proof.copy_source,
        ) {
            Ok(true) => {}
            Ok(false) => warn!(
                "draft {}: Sent-copy proof not recorded (row is not `sent`)",
                draft.id
            ),
            Err(e) => warn!("draft {}: failed to record Sent-copy proof: {e}", draft.id),
        }
    }

    Ok(SentAttempt {
        draft_id: draft.id.clone(),
        message_id: header_id,
        attribution: gov.success_attribution(),
        copy,
        imap_draft_deleted,
        warnings,
    })
}

/// The Governor gate for a claimed row, run on what will be transmitted. A
/// refusal releases the claim.
fn gate_claimed(
    db: &Database,
    creds: &AccountWithCredentials,
    draft: &Draft,
    outgoing: &Outgoing,
    surface: &AttemptSurface<'_>,
    guard: &mut ClaimGuard<'_>,
) -> Result<GovernorOutcome> {
    let gov_req = governor_request(
        db,
        &draft.account_id,
        account_domain(&creds.account.username),
        &outgoing.subject,
        &outgoing.to,
        outgoing.cc.as_deref(),
        outgoing.bcc.as_deref(),
        surface.governor,
        Some(&draft.id),
        &outgoing.attachments,
        outgoing.in_reply_to.as_deref(),
        outgoing.text.as_deref(),
        outgoing.html.as_deref(),
        surface.declared,
    );
    let gov = gate_and_record_with_agent(db, &draft.account_id, &gov_req, surface.agent_id)?;
    if !gov.allowed {
        let reason = format!(
            "governor_{}",
            gov.block_code.as_deref().unwrap_or("governor_blocked")
        );
        guard.release(&reason, None)?;
        return Err(GovernorRefused { outcome: gov }.into());
    }
    Ok(gov)
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::models::Account;
    use envelope_email_transport::smtp_submit::testing::{PlainConnector, Script, ScriptedServer};

    struct Fixture {
        _dir: tempfile::TempDir,
        db: Database,
        creds: AccountWithCredentials,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("envelope.db")).unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'Sender', 'sender@example.test', 'example.test',
                         'smtp.example.test', 587, '', 993, 'encrypted')",
                [],
            )
            .unwrap();
        let creds = AccountWithCredentials {
            account: Account {
                id: "acc1".into(),
                name: "Sender".into(),
                username: "sender@example.test".into(),
                domain: "example.test".into(),
                smtp_host: "smtp.example.test".into(),
                smtp_port: 587,
                // No IMAP: the Sent-copy step is skipped, so each test is
                // about SMTP and the local record only.
                imap_host: String::new(),
                imap_port: 993,
                smtp_username: None,
                imap_username: None,
                display_name: None,
                signature_text: None,
                signature_html: None,
                created_at: String::new(),
            },
            password: "pw".into(),
            smtp_password: None,
            imap_password: None,
        };
        Fixture {
            _dir: dir,
            db,
            creds,
        }
    }

    const DECLARED: &[String] = &[];

    fn request(key: Option<&str>) -> SendRequest<'_> {
        SendRequest {
            surface: SendSurface::Cli,
            label: "cli_send",
            principal: "local".into(),
            agent_id: None,
            idempotency_key: key,
            to: "alice@example.test",
            cc: None,
            bcc: None,
            reply_to: None,
            subject: "Crash test",
            text: Some("Line one\nLine two"),
            html: None,
            from: None,
            in_reply_to: None,
            references: &[],
            attachments: &[],
            declared: DECLARED,
            metadata: json!({}),
            created_by: "cli",
        }
    }

    fn count(db: &Database, sql: &str) -> i64 {
        db.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// The gate runs on the request before anything is recorded: a refused
    /// request (here, no declared attribute) leaves no row and reaches no
    /// server.
    #[tokio::test]
    async fn a_refused_request_leaves_no_row() {
        let f = fixture();
        let server = ScriptedServer::start(Script::default()).await;
        let connector = PlainConnector { addr: server.addr };
        let req = request(None);

        let err = send_now(&f.db, &f.creds, &req, &connector)
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<GovernorRefused>().is_some(), "{err:#}");
        assert_eq!(count(&f.db, "SELECT COUNT(*) FROM drafts"), 0);
        assert!(!server.saw("MAIL"));
    }

    /// Without `--json` the error is all a person sees, and its advice
    /// ("discard this draft") needs the draft's id.
    #[test]
    fn a_human_readable_outcome_names_its_draft() {
        let outcome = SendNotConfirmed {
            body: json!({
                "status": "delivery_uncertain",
                "draft_id": "d-9",
                "error": {"reason": "It may have been delivered."},
            }),
        };
        assert_eq!(
            outcome.to_string(),
            "It may have been delivered. (draft d-9)"
        );
    }

    /// Sends the gate lets through. A `governor` build gates every send on the
    /// trusted Governor binary, which a test machine may not have (CI has none),
    /// so these run where the gate is compiled off. The attribution refusal is
    /// covered in every build by `a_refused_request_leaves_no_row`.
    #[cfg(not(feature = "governor"))]
    mod allowed {
        use super::*;
        use envelope_email_transport::smtp_submit::testing::AfterBody;
        use std::time::Duration;

        fn declared() -> Vec<String> {
            vec!["informational".to_string()]
        }

        fn with_declared<'a>(mut req: SendRequest<'a>, attrs: &'a [String]) -> SendRequest<'a> {
            req.declared = attrs;
            req
        }

        fn status_of(err: &anyhow::Error) -> Value {
            err.downcast_ref::<SendNotConfirmed>()
                .map(|e| e.body.clone())
                .unwrap_or_else(|| panic!("not a structured send outcome: {err:#}"))
        }

        async fn wait_for_body(server: &ScriptedServer) {
            while server.bodies() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        /// Pilot failure `send_now/after_data/kill`: the process died after the
        /// server had the whole message, and the rerun of the same command sent it
        /// again (30/30 duplicates). Dropping the send future mid-reply is that
        /// kill: every piece of state is in SQLite, and the owner lock is released
        /// as the kernel would release it.
        #[tokio::test]
        async fn a_rerun_after_a_crash_mid_body_does_not_send_again() {
            let f = fixture();
            let attrs = declared();
            let server = ScriptedServer::start(Script {
                after_body: AfterBody::Hang,
                ..Script::default()
            })
            .await;
            let connector = PlainConnector { addr: server.addr };
            let req = with_declared(request(None), &attrs);

            tokio::select! {
                result = send_now(&f.db, &f.creds, &req, &connector) => {
                    panic!("the server never answers the body: {result:?}")
                }
                _ = wait_for_body(&server) => {}
            }

            let rerun = send_now(&f.db, &f.creds, &req, &connector).await;
            let body = status_of(&rerun.unwrap_err());
            assert_eq!(body["status"], "delivery_uncertain", "{body}");
            assert_eq!(body["retryable"], false);
            assert_eq!(server.bodies(), 1, "the rerun must not transmit again");
            assert_eq!(count(&f.db, "SELECT COUNT(*) FROM drafts"), 1);
        }

        /// Pilot failures `send_now/after_250/kill` and `after_append/kill`: the
        /// server accepted, the process died before or during the Sent copy, and
        /// the rerun sent a second copy. The rerun must answer from the record.
        #[tokio::test]
        async fn a_rerun_after_acceptance_replays_the_recorded_send() {
            let f = fixture();
            let attrs = declared();
            let server = ScriptedServer::start(Script::default()).await;
            let connector = PlainConnector { addr: server.addr };
            let req = with_declared(request(None), &attrs);

            let first = send_now(&f.db, &f.creds, &req, &connector).await.unwrap();
            assert_eq!(first["status"], "sent");
            let rerun = send_now(&f.db, &f.creds, &req, &connector).await.unwrap();
            assert_eq!(rerun["status"], "sent");
            assert_eq!(rerun["idempotent_replay"], true);
            assert_eq!(rerun["message_id"], first["message_id"]);
            assert_eq!(rerun["draft_id"], first["draft_id"]);
            assert_eq!(server.bodies(), 1);
        }

        /// A connection that never got to the body sent nothing: the intent is
        /// released and the rerun sends it exactly once.
        #[tokio::test]
        async fn a_rerun_after_an_unreachable_server_sends_once() {
            let f = fixture();
            let attrs = declared();
            let req = with_declared(request(None), &attrs);
            let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let dead = PlainConnector {
                addr: closed.local_addr().unwrap(),
            };
            drop(closed);

            let failed = status_of(&send_now(&f.db, &f.creds, &req, &dead).await.unwrap_err());
            assert_eq!(failed["status"], "not_sent");
            assert_eq!(failed["retryable"], true);

            let server = ScriptedServer::start(Script::default()).await;
            let live = PlainConnector { addr: server.addr };
            let sent = send_now(&f.db, &f.creds, &req, &live).await.unwrap();
            assert_eq!(sent["status"], "sent");
            assert_eq!(sent["draft_id"], failed["draft_id"]);
            assert_eq!(server.bodies(), 1);
            assert_eq!(count(&f.db, "SELECT COUNT(*) FROM drafts"), 1);
        }

        /// A body the server refused was not accepted: the intent goes back to
        /// `draft`, never `delivery_uncertain`, and says whether a retry can help.
        #[tokio::test]
        async fn a_refused_body_is_released_with_its_reply_code() {
            let f = fixture();
            let attrs = declared();
            let server = ScriptedServer::start(Script {
                after_body: AfterBody::Reply("554 5.7.1 rejected"),
                ..Script::default()
            })
            .await;
            let connector = PlainConnector { addr: server.addr };
            let req = with_declared(request(None), &attrs);

            let body = status_of(
                &send_now(&f.db, &f.creds, &req, &connector)
                    .await
                    .unwrap_err(),
            );
            assert_eq!(body["status"], "not_sent");
            assert_eq!(body["retryable"], false);
            assert_eq!(body["error"]["reply_code"], 554);
            assert_eq!(
                count(&f.db, "SELECT COUNT(*) FROM drafts WHERE status = 'draft'"),
                1
            );
        }

        /// Pilot finding: no `action_log` rows at all, and `send_completed` with a
        /// NULL `message_id`. Every transition of an immediate send leaves a
        /// receipt tying the operation to its recipients, content and outcome.
        #[tokio::test]
        async fn an_immediate_send_leaves_receipts_for_every_transition() {
            let f = fixture();
            let attrs = declared();
            let server = ScriptedServer::start(Script::default()).await;
            let connector = PlainConnector { addr: server.addr };
            let req = with_declared(request(None), &attrs);

            let sent = send_now(&f.db, &f.creds, &req, &connector).await.unwrap();
            let draft_id = sent["draft_id"].as_str().expect("draft_id").to_string();
            let message_id = sent["message_id"].as_str().expect("message_id").to_string();

            let mut stmt =
                f.db.conn()
                    .prepare(
                        "SELECT action_status, message_id, action_taken FROM action_log
                     WHERE action_type = 'send' AND draft_id = ?1 ORDER BY rowid",
                    )
                    .unwrap();
            let rows: Vec<(String, Option<String>, Value)> = stmt
                .query_map([&draft_id], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        serde_json::from_str(&r.get::<_, String>(2)?).unwrap(),
                    ))
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            let statuses: Vec<(&str, &str)> = rows
                .iter()
                .map(|(s, _, t)| (s.as_str(), t["phase"].as_str().unwrap()))
                .collect();
            assert_eq!(
                statuses,
                vec![
                    ("sending", "claimed"),
                    ("sending", "transmitting"),
                    ("sent", "accepted")
                ]
            );
            for (_, mid, taken) in &rows {
                assert_eq!(mid.as_deref(), Some(message_id.as_str()));
                assert_eq!(taken["recipients"], json!(["alice@example.test"]));
                assert_eq!(
                    taken["semantic_sha256"],
                    "523f0d8bc7e76d999b5ce167fed8429b8e54365cd26984d1b84fc28f5594881e"
                );
            }
            let completed: Option<String> =
                f.db.conn()
                    .query_row(
                        "SELECT message_id FROM events WHERE event_type = 'send_completed'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
            assert_eq!(
                completed.as_deref(),
                Some(message_id.trim_matches(|c| c == '<' || c == '>'))
            );
        }

        /// Two processes ran the same request: the other one claimed the intent
        /// between this one's lookup and its claim, and finished the send. This
        /// request reports the recorded send, as a later rerun would, instead of
        /// failing with `draft_changed`.
        #[tokio::test]
        async fn losing_the_claim_to_a_finished_send_replays_it() {
            let f = fixture();
            let attrs = declared();
            let server = ScriptedServer::start(Script::default()).await;
            let connector = PlainConnector { addr: server.addr };
            let req = with_declared(request(None), &attrs);
            let first = send_now(&f.db, &f.creds, &req, &connector).await.unwrap();
            let draft_id = first["draft_id"].as_str().expect("draft_id");

            let lost = claim_lost(&f.db, &req, draft_id).expect("a sent intent is replayed");
            assert_eq!(lost["status"], "sent", "{lost}");
            assert_eq!(lost["idempotent_replay"], true);
            assert_eq!(lost["message_id"], first["message_id"]);
            assert_eq!(server.bodies(), 1);
        }

        /// The same explicit key with different content sends nothing.
        #[tokio::test]
        async fn an_explicit_key_with_new_content_is_refused() {
            let f = fixture();
            let attrs = declared();
            let server = ScriptedServer::start(Script::default()).await;
            let connector = PlainConnector { addr: server.addr };
            let first = with_declared(request(Some("op-1")), &attrs);
            send_now(&f.db, &f.creds, &first, &connector).await.unwrap();

            let mut drifted = with_declared(request(Some("op-1")), &attrs);
            drifted.text = Some("Something else");
            let body = status_of(
                &send_now(&f.db, &f.creds, &drifted, &connector)
                    .await
                    .unwrap_err(),
            );
            assert_eq!(body["status"], "idempotency_key_conflict");
            assert_eq!(server.bodies(), 1);
        }
    }
}
