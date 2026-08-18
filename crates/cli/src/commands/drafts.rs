// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::Database;
use envelope_email_store::credential_store::{self, CredentialBackend};
use envelope_email_store::models::{AccountWithCredentials, AttachmentMeta, Draft};
use envelope_email_transport::SmtpSender;
use envelope_email_transport::compose::{
    self, ContextBlock, DEFAULT_PREVIEW_WORD_LIMIT, DraftKind,
};
use envelope_email_transport::detect_drafts_folder;
use envelope_email_transport::imap;
use envelope_email_transport::outbound::SendSurface;
use envelope_email_transport::reply;
use envelope_email_transport::smtp::Attachment;
use lettre::message::Mailboxes;
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as BuilderAddress;
use tracing::{info, warn};

use super::attachments::{attachment_summaries, decode_attachments, snapshot_attachments};
use super::common::{resolve_account, setup_credentials};
use super::re_subject_guard::check_new_re_subject_guard;
use super::ui;

// The source-aware Sent-folder proof resolver is shared with the scheduled-send
// sweep (which lives in the dashboard crate) so every real SMTP path — immediate
// CLI/MCP and the background sweep alike — resolves Sent copies through the SAME
// implementation. Re-exported so existing `crate::commands::drafts::*` call sites
// keep resolving to it.
#[cfg(test)]
pub(crate) use envelope_email_transport::sent_proof::{
    SentCopyDecision, decide_sent_copy_action, determine_copy_source, provider_auto_saves_sent,
};
pub(crate) use envelope_email_transport::sent_proof::{
    SentMailProof, resolve_sent_copy_after_send,
};

/// CLI presentation for a [`SentMailProof`]: the dashboard message URL and UI
/// block. Kept in the CLI (not the transport crate) because it depends on the
/// CLI `ui` routing helpers.
pub(crate) trait SentMailProofUi {
    fn message_url(&self, account_id: &str) -> Option<String>;
    fn ui(&self, account_id: &str) -> serde_json::Value;
}

impl SentMailProofUi for SentMailProof {
    fn message_url(&self, account_id: &str) -> Option<String> {
        let folder = self.folder.as_deref()?;
        let uid = self.uid?;
        ui::message_ui(account_id, uid, folder)
            .get("message_url")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    fn ui(&self, account_id: &str) -> serde_json::Value {
        match (self.folder.as_deref(), self.uid) {
            (Some(folder), Some(uid)) => ui::message_ui(account_id, uid, folder),
            _ => ui::account_ui(account_id),
        }
    }
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub(crate) fn draft_dashboard_path(account_id: &str, draft_id: &str) -> String {
    format!(
        "/accounts/{}/drafts/{}",
        encode_path_segment(account_id),
        encode_path_segment(draft_id)
    )
}

fn draft_dashboard_url_with_base(base_url: &str, account_id: &str, draft_id: &str) -> String {
    format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        draft_dashboard_path(account_id, draft_id)
    )
}

pub(crate) fn draft_dashboard_url(account_id: &str, draft_id: &str) -> String {
    // Resolve through the canonical dashboard base URL precedence
    // (ENVELOPE_DASHBOARD_BASE_URL → legacy alias → persisted dashboard.base_url
    // → localhost) so top-level draft URLs match the nested `ui` review URLs.
    draft_dashboard_url_with_base(&ui::dashboard_base(), account_id, draft_id)
}

/// Strip surrounding angle brackets from a Message-ID (`<id>` → `id`).
pub(crate) fn strip_brackets(s: &str) -> String {
    s.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

pub(crate) fn sent_mail_proof_json(account_id: &str, proof: &SentMailProof) -> serde_json::Value {
    serde_json::json!({
        "folder": proof.folder,
        "uid": proof.uid,
        "message_url": proof.message_url(account_id),
        "lookup_status": proof.lookup_status,
        "lookup_error": proof.lookup_error,
        "copy_source": proof.copy_source,
        "ui": proof.ui(account_id),
    })
}

/// Project a resolved [`SentMailProof`] into the two convenience proof objects
/// (`provider_sent_copy`, `client_appended_copy`) that every CLI/MCP actual-send
/// output advertises. Populated STRICTLY by `copy_source`:
/// - `provider_sent_copy` only when the copy was observed provider-side
///   (`copy_source == "provider"`);
/// - `client_appended_copy` only for a client IMAP-APPEND archive
///   (`copy_source == "client_appended"`).
///
/// `unresolved` and `not_attempted` yield `None` for both — an unresolved lookup
/// is never presented as provider proof (upholding the contract's
/// generic-provider-null guarantee). The canonical `sent_mail` object still
/// truthfully exposes `copy_source`, folder, uid, and lookup status. Centralized
/// so the four actual-send call sites (CLI send, MCP send/reply, durable draft
/// send) cannot drift.
pub(crate) fn sent_copy_convenience_objects(
    account_id: &str,
    proof: &SentMailProof,
) -> (Option<serde_json::Value>, Option<serde_json::Value>) {
    let provider_sent_copy =
        (proof.copy_source == "provider").then(|| sent_mail_proof_json(account_id, proof));
    let client_appended_copy =
        (proof.copy_source == "client_appended").then(|| sent_mail_proof_json(account_id, proof));
    (provider_sent_copy, client_appended_copy)
}

/// Build a full RFC822 draft supporting HTML, References, In-Reply-To, and an
/// explicit (preserved) Message-ID.
///
/// Passing `message_id = Some(bare_id)` preserves a stable Message-ID across
/// draft modify/send cycles; passing `None` lets mail-builder generate one.
/// Returns `(rfc822_bytes, message_id_header_value)` where the returned value
/// includes angle brackets as written to the message.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_rfc822_full(
    from: &str,
    to: &str,
    subject: &str,
    text: Option<&str>,
    html: Option<&str>,
    cc: Option<&str>,
    in_reply_to: Option<&str>,
    references: &[String],
    message_id: Option<&str>,
    attachments: &[Attachment],
) -> Result<(Vec<u8>, String)> {
    let mut builder = MessageBuilder::new()
        .from(builder_from_address(from)?)
        .subject(subject);
    if !to.trim().is_empty() {
        builder = builder.to(builder_address_list(to, "to")?);
    }

    if let Some(cc_addr) = cc {
        if !cc_addr.trim().is_empty() {
            builder = builder.cc(builder_address_list(cc_addr, "cc")?);
        }
    }
    if let Some(irt) = in_reply_to {
        if !irt.trim().is_empty() {
            builder = builder.in_reply_to(strip_brackets(irt));
        }
    }
    if !references.is_empty() {
        let bare: Vec<String> = references.iter().map(|r| strip_brackets(r)).collect();
        builder = builder.references(bare);
    }
    if let Some(mid) = message_id {
        if !mid.trim().is_empty() {
            builder = builder.message_id(strip_brackets(mid));
        }
    }

    builder = match (text, html) {
        (Some(t), Some(h)) => builder.text_body(t).html_body(h),
        (Some(t), None) => builder.text_body(t),
        (None, Some(h)) => builder.html_body(h),
        (None, None) => builder.text_body(""),
    };

    for att in attachments {
        builder = builder.attachment(
            att.content_type.clone(),
            att.filename.clone(),
            att.data.clone(),
        );
    }

    let rfc822 = builder
        .write_to_string()
        .context("failed to build RFC822 message")?;

    let message_id = rfc822
        .lines()
        .find(|l| l.to_lowercase().starts_with("message-id:"))
        .map(|l| {
            l.split_once(':')
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    Ok((compose::normalize_crlf(rfc822.as_bytes()), message_id))
}

/// Build an RFC822-formatted draft message suitable for IMAP APPEND.
///
/// Returns (rfc822_bytes, message_id).
fn build_rfc822_draft(
    from: &str,
    to: &str,
    subject: Option<&str>,
    body: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    in_reply_to: Option<&str>,
    attachments: &[Attachment],
) -> Result<(Vec<u8>, String)> {
    let mut builder = MessageBuilder::new()
        .from(builder_from_address(from)?)
        .subject(subject.unwrap_or(""));

    if !to.trim().is_empty() {
        builder = builder.to(builder_address_list(to, "to")?);
    }

    if let Some(cc_addr) = cc {
        if !cc_addr.trim().is_empty() {
            builder = builder.cc(builder_address_list(cc_addr, "cc")?);
        }
    }

    if let Some(bcc_addr) = bcc {
        if !bcc_addr.trim().is_empty() {
            builder = builder.bcc(builder_address_list(bcc_addr, "bcc")?);
        }
    }

    if let Some(irt) = in_reply_to {
        builder = builder.in_reply_to(irt);
    }

    let text = body.unwrap_or("");
    builder = builder.text_body(text);

    for att in attachments {
        builder = builder.attachment(
            att.content_type.clone(),
            att.filename.clone(),
            att.data.clone(),
        );
    }

    let rfc822 = builder
        .write_to_string()
        .context("failed to build RFC822 message")?;

    // Extract the Message-ID from the generated RFC822
    let message_id = rfc822
        .lines()
        .find(|l| l.to_lowercase().starts_with("message-id:"))
        .map(|l| {
            l.split_once(':')
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    Ok((compose::normalize_crlf(rfc822.as_bytes()), message_id))
}

/// Convert an RFC5322 mailbox list into `mail-builder`'s address-list type.
///
/// `mail-builder` treats a raw string as one mailbox, so passing
/// `"a@example.com, b@example.com"` directly produces an invalid single address.
/// Parse with lettre's RFC5322 mailbox-list parser first, then hand
/// mail-builder an explicit list so draft RFC822 matches SMTP send behavior.
/// Parse a `From:` header value into a single mail-builder address.
///
/// The incoming `from` string is already a fully-formed RFC5322 mailbox — either
/// the account default produced by [`account_from_header`] (e.g.
/// `"Display Name" <user@example.test>`) or an explicit `--from` override. It must
/// be parsed into `(display_name, email)` parts so mail-builder can re-serialize
/// it safely; passing the preformatted string to `MessageBuilder::from` treats the
/// whole thing as a bare address and double-wraps it into `<Display Name <addr>>`
/// (issue #81).
fn builder_from_address(from: &str) -> Result<BuilderAddress<'static>> {
    let mailboxes = from
        .parse::<Mailboxes>()
        .with_context(|| "invalid from address")?;
    let mailbox = mailboxes
        .iter()
        .next()
        .with_context(|| "from address is empty")?;
    Ok(BuilderAddress::new_address(
        mailbox.name.clone(),
        mailbox.email.to_string(),
    ))
}

fn builder_address_list(value: &str, field: &str) -> Result<BuilderAddress<'static>> {
    let mailboxes = value
        .parse::<Mailboxes>()
        .with_context(|| format!("invalid {field} address"))?;
    let items = mailboxes
        .iter()
        .map(|mailbox| BuilderAddress::new_address(mailbox.name.clone(), mailbox.email.to_string()))
        .collect::<Vec<_>>();
    Ok(BuilderAddress::new_list(items))
}

/// Threading + preserved Message-ID pulled from a local draft's metadata blob.
///
/// Returns `(in_reply_to, references, message_id)`. Contextual reply drafts
/// store these so send-by-draft-id can re-emit `In-Reply-To`/`References`
/// without re-fetching the parent.
pub(crate) fn threading_from_metadata(
    metadata: Option<&serde_json::Value>,
) -> (Option<String>, Vec<String>, Option<String>) {
    let Some(meta) = metadata else {
        return (None, Vec::new(), None);
    };
    let in_reply_to = meta
        .get("in_reply_to")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let references = meta
        .get("references")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let message_id = meta
        .get("message_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    (in_reply_to, references, message_id)
}

/// Threading for a stored draft, reconciling the two places it can live.
///
/// The contextual reply/forward builders write a metadata blob; every other
/// path that creates a reply (IMAP-synced drafts, plain `draft create`) sets
/// only the `in_reply_to` column. Reading metadata alone made column-only
/// replies look like fresh messages, so metadata wins where present and the
/// columns are the fallback.
pub(crate) fn threading_for_draft(draft: &Draft) -> (Option<String>, Vec<String>, Option<String>) {
    let (in_reply_to, references, message_id) = threading_from_metadata(draft.metadata.as_ref());
    (
        in_reply_to.or_else(|| draft.in_reply_to.clone()),
        references,
        message_id.or_else(|| draft.message_id.clone()),
    )
}

// ─── contextual reply / forward drafts ───────────────────────────────────

/// Build the `From:` header value using the same precedence as SMTP:
/// explicit `display_name` → non-empty `account.name` (when not identical to the
/// email address) → bare email address.
///
/// Uses `lettre::message::Mailbox` for RFC5322-safe quoting so names containing
/// commas or other special characters are quoted correctly.
pub(crate) fn account_from_header(creds: &AccountWithCredentials) -> String {
    use lettre::{Address, message::Mailbox};

    let address = creds.account.username.trim();
    let display_name = creds
        .account
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .or_else(|| {
            let name = creds.account.name.trim();
            if !name.is_empty() && !name.eq_ignore_ascii_case(address) {
                Some(name)
            } else {
                None
            }
        });

    if let Ok(email) = address.parse::<Address>() {
        let mbox = Mailbox::new(display_name.map(str::to_string), email);
        return mbox.to_string();
    }

    // Fallback for malformed addresses: use raw string concatenation.
    match display_name {
        Some(name) => format!("{name} <{address}>"),
        None => address.to_string(),
    }
}

/// Reconstruct the preserved [`ContextBlock`] from a draft's metadata blob.
fn context_from_metadata(meta: &serde_json::Value) -> ContextBlock {
    let c = meta.get("context");
    ContextBlock {
        text: c
            .and_then(|c| c.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        html: c
            .and_then(|c| c.get("html"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        format: c
            .and_then(|c| c.get("format"))
            .and_then(|v| v.as_str())
            .unwrap_or("plain_prefix")
            .to_string(),
        included: c
            .and_then(|c| c.get("included"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// APPEND an RFC822 draft to the account's Drafts folder, best-effort.
///
/// Returns `(imap_synced, drafts_folder, imap_uid)`. Connection/append/detect
/// failures degrade to local-only with a warning (mirroring `run_create`) and
/// never abort the draft.
async fn append_draft_best_effort(
    db: &Database,
    creds: &AccountWithCredentials,
    rfc822: &[u8],
    message_id: &str,
) -> (bool, String, Option<u32>) {
    let drafts_folder = String::from("Drafts");
    if creds.account.imap_host.is_empty() {
        warn!(
            "account {} has no IMAP — draft saved locally only (send-only account)",
            creds.account.username
        );
        return (false, drafts_folder, None);
    }
    let mut client = match imap::connect(creds).await {
        Ok(c) => c,
        Err(e) => {
            warn!("IMAP connect failed: {e}; saving draft locally only");
            return (false, drafts_folder, None);
        }
    };
    let folder = match detect_drafts_folder(&mut client, db, &creds.account.id).await {
        Ok(Some(folder)) => folder,
        Ok(None) => {
            warn!("no drafts folder detected; saving draft locally only");
            return (false, drafts_folder, None);
        }
        Err(e) => {
            warn!("drafts folder detection failed: {e}; saving draft locally only");
            return (false, drafts_folder, None);
        }
    };
    if let Err(e) = imap::append_message(&mut client, &folder, "(\\Draft \\Seen)", rfc822).await {
        warn!("IMAP APPEND to {folder} failed: {e}");
        return (false, folder, None);
    }
    let uid = if message_id.is_empty() {
        None
    } else {
        let mid_clean = message_id.trim_matches(|c| c == '<' || c == '>');
        match imap::find_uid_by_message_id(&mut client, &folder, mid_clean).await {
            Ok(u) => u,
            Err(e) => {
                warn!("IMAP APPEND succeeded but UID lookup failed: {e}");
                None
            }
        }
    };
    (true, folder, uid)
}

/// APPEND a draft to the account's IMAP Drafts folder, or fail loud.
///
/// A draft's real home is the account's Drafts folder — the store Mail.app and
/// every other IMAP client display. Envelope's local SQLite record is a
/// secondary index, invisible to those clients, so we never silently downgrade
/// to a local-only draft: any failure (no IMAP host, connect, folder
/// detection, or the APPEND itself) returns an error so the caller aborts
/// before creating a phantom local record. The Drafts folder is resolved by
/// Envelope (SPECIAL-USE `\Drafts`, then provider/name detection); the caller
/// never names it.
///
/// Returns `(drafts_folder, appended_uid)`. A missing UID (post-APPEND search
/// miss) is non-fatal — the draft is on the server regardless.
async fn append_draft_required(
    db: &Database,
    creds: &AccountWithCredentials,
    rfc822: &[u8],
    message_id: &str,
) -> Result<(String, Option<u32>)> {
    if creds.account.imap_host.is_empty() {
        bail!(
            "account {} has no IMAP mailbox, so a draft has nowhere to land where your \
             mail client can see it. Drafts require an IMAP account.",
            creds.account.username
        );
    }
    let mut client = imap::connect(creds)
        .await
        .context("connecting to IMAP to save the draft")?;
    let folder = match detect_drafts_folder(&mut client, db, &creds.account.id).await? {
        Some(folder) => folder,
        None => {
            // Every real mailbox has a Drafts folder, and detection now covers
            // SPECIAL-USE + provider + localized names. If we still find none,
            // create the canonical one as a last resort so the draft lands.
            let target = String::from("Drafts");
            imap::create_folder_if_missing(&mut client, &target)
                .await
                .context("no Drafts folder found and creating one failed")?;
            if let Err(e) = db.set_detected_folder(&creds.account.id, "drafts", &target) {
                warn!("failed to cache drafts folder: {e}");
            }
            info!(
                "no Drafts folder detected for {}; created '{target}'",
                creds.account.username
            );
            target
        }
    };
    imap::append_message(&mut client, &folder, "(\\Draft \\Seen)", rfc822)
        .await
        .with_context(|| format!("appending the draft to your '{folder}' folder"))?;
    let uid = if message_id.is_empty() {
        None
    } else {
        let mid_clean = message_id.trim_matches(|c| c == '<' || c == '>');
        imap::find_uid_by_message_id(&mut client, &folder, mid_clean)
            .await
            .unwrap_or(None)
    };
    Ok((folder, uid))
}

/// All the resolved fields needed to instantiate a contextual draft.
///
/// Built once by [`run_reply`]/[`run_forward`] and consumed by
/// [`create_contextual_draft`]. Using a struct keeps the helper off the
/// `too_many_arguments` lint and documents each field at the call site.
struct ContextualDraftSpec {
    kind: DraftKind,
    source_folder: String,
    source_uid: u32,
    source_message_id: Option<String>,
    to: String,
    cc: Option<String>,
    bcc: Option<String>,
    subject: String,
    in_reply_to: Option<String>,
    references: Vec<String>,
    agent_text: String,
    agent_html: Option<String>,
    signature: bool,
    context: ContextBlock,
    /// Source body used to compute the abridged preview.
    preview_source: String,
    /// New attachments explicitly added to this contextual draft.
    attachment_snapshots: Vec<serde_json::Value>,
    attachments: Vec<Attachment>,
    attachments_forwarded: bool,
}

/// Assemble the full body, build RFC822, APPEND to IMAP Drafts, persist the
/// local draft record + contextual metadata, and return the stored draft.
async fn create_contextual_draft(
    db: &Database,
    creds: &AccountWithCredentials,
    spec: ContextualDraftSpec,
) -> Result<Draft> {
    let (sig_text, sig_html) = if spec.signature {
        (
            creds.account.signature_text.as_deref(),
            creds.account.signature_html.as_deref(),
        )
    } else {
        (None, None)
    };
    let assembled = compose::assemble_body(
        &spec.agent_text,
        spec.agent_html.as_deref(),
        sig_text,
        sig_html,
        spec.signature,
        &spec.context,
    );

    let from = account_from_header(creds);
    let (rfc822, message_id_hdr) = build_rfc822_full(
        &from,
        &spec.to,
        &spec.subject,
        Some(&assembled.text),
        assembled.html.as_deref(),
        spec.cc.as_deref(),
        spec.in_reply_to.as_deref(),
        &spec.references,
        None,
        &spec.attachments,
    )?;

    // Mandatory IMAP APPEND: the reply/forward draft must land in the account's
    // real Drafts folder (visible in Mail.app and every IMAP client). On any
    // failure this bails before the local record is created — no phantom.
    let (imap_folder, imap_uid) =
        append_draft_required(db, creds, &rfc822, &message_id_hdr).await?;

    // Local record (full assembled body is the source of truth for the quote).
    let draft = db
        .create_draft(
            &creds.account.id,
            &spec.to,
            Some(&spec.subject),
            Some(&assembled.text),
            assembled.html.as_deref(),
            spec.in_reply_to.as_deref(),
            spec.cc.as_deref(),
            spec.bcc.as_deref(),
            Some("cli"),
        )
        .context("failed to create local draft record")?;

    if let Some(uid) = imap_uid {
        if let Err(e) = db.update_draft_imap_uid(&draft.id, uid) {
            warn!("failed to store IMAP UID in local DB: {e}");
        }
    }
    let bare_message_id = strip_brackets(&message_id_hdr);
    if !bare_message_id.is_empty() {
        let _ = db.mark_draft_message_id(&draft.id, &bare_message_id);
    }
    if !spec.attachment_snapshots.is_empty() {
        db.update_draft_attachments(&draft.id, &spec.attachment_snapshots)
            .context("failed to persist draft attachments")?;
    }

    let (preview_text, preview_truncated) =
        compose::abridge_words(&spec.preview_source, DEFAULT_PREVIEW_WORD_LIMIT);

    let metadata = serde_json::json!({
        "draft_kind": spec.kind.as_str(),
        "source": {
            "folder": spec.source_folder,
            "uid": spec.source_uid,
            "message_id": spec.source_message_id,
        },
        "in_reply_to": spec.in_reply_to,
        "references": spec.references,
        "message_id": bare_message_id,
        "context": {
            "text": spec.context.text,
            "html": spec.context.html,
            "format": spec.context.format,
            "included": spec.context.included,
        },
        "quote_format": spec.context.format,
        "agent_body_text": spec.agent_text,
        "agent_body_html": spec.agent_html,
        "signature_applied": assembled.signature_applied,
        "preview_text": preview_text,
        "preview_truncated": preview_truncated,
        "preview_word_limit": DEFAULT_PREVIEW_WORD_LIMIT,
        "attachments_forwarded": spec.attachments_forwarded,
        "full_content_preserved": true,
        "storage": {
            "imap_synced": true,
            "imap_folder": imap_folder,
            "local_only": false,
        },
    });
    db.set_draft_metadata(&draft.id, &metadata)
        .context("failed to persist draft metadata")?;

    db.get_draft(&draft.id)
        .context("failed to reload draft")?
        .ok_or_else(|| anyhow::anyhow!("draft vanished after creation: {}", draft.id))
}

/// Build a contextual reply draft. Shared by the CLI and MCP surfaces.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_reply_draft(
    db: &Database,
    creds: &AccountWithCredentials,
    uid: u32,
    folder: &str,
    reply_all: bool,
    body: Option<&str>,
    html: Option<&str>,
    signature: bool,
    attach_paths: &[String],
) -> Result<Draft> {
    if creds.account.imap_host.is_empty() {
        bail!("reply requires an IMAP account to fetch the parent message");
    }
    let parent = {
        let mut client = imap::connect(creds)
            .await
            .context("failed to connect to IMAP")?;
        imap::fetch_message(&mut client, folder, uid)
            .await
            .context("failed to fetch parent message")?
            .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?
    };

    let headers = if reply_all {
        reply::build_reply_all_headers(&parent, &creds.account.username)
    } else {
        reply::build_reply_headers(&parent)
    };
    let cc = if headers.cc.is_empty() {
        None
    } else {
        Some(headers.cc.join(", "))
    };
    let attachment_snapshots = snapshot_attachments(attach_paths)?;
    let attachments = decode_attachments(&attachment_snapshots)?;

    let spec = ContextualDraftSpec {
        kind: DraftKind::Reply,
        source_folder: folder.to_string(),
        source_uid: uid,
        source_message_id: parent.message_id.clone(),
        to: headers.to,
        cc,
        bcc: None,
        subject: headers.subject,
        in_reply_to: headers.in_reply_to,
        references: headers.references,
        agent_text: body.unwrap_or("").to_string(),
        agent_html: html.map(str::to_string),
        signature,
        context: compose::build_reply_context(&parent),
        preview_source: compose::message_preview_source(&parent),
        attachment_snapshots,
        attachments,
        attachments_forwarded: false,
    };
    create_contextual_draft(db, creds, spec).await
}

/// Snapshot original source-message attachments for explicit forward-with-attachments.
///
/// This is intentionally opt-in because forwarding source attachments can move
/// sensitive/large files. The output uses the same draft attachment JSON shape as
/// CLI `--attach`: metadata plus a base64 payload for later draft send.
async fn snapshot_source_attachments(
    creds: &AccountWithCredentials,
    uid: u32,
    folder: &str,
    source_attachments: &[AttachmentMeta],
) -> Result<Vec<serde_json::Value>> {
    use base64::Engine as _;

    if source_attachments.is_empty() {
        return Ok(Vec::new());
    }

    let mut client = imap::connect(creds)
        .await
        .context("failed to connect to IMAP for source attachments")?;
    let mut snapshots = Vec::with_capacity(source_attachments.len());
    for meta in source_attachments {
        let (filename, data) = imap::download_attachment(&mut client, uid, &meta.filename, folder)
            .await
            .with_context(|| format!("failed to download source attachment: {}", meta.filename))?;
        snapshots.push(serde_json::json!({
            "filename": filename,
            "content_type": meta.content_type,
            "size": data.len(),
            "data_base64": base64::engine::general_purpose::STANDARD.encode(&data),
        }));
    }
    Ok(snapshots)
}

/// Build a contextual forward draft. Shared by the CLI and MCP surfaces.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_forward_draft(
    db: &Database,
    creds: &AccountWithCredentials,
    uid: u32,
    folder: &str,
    to: Option<&str>,
    body: Option<&str>,
    html: Option<&str>,
    signature: bool,
    attach_paths: &[String],
    include_attachments: bool,
) -> Result<Draft> {
    if creds.account.imap_host.is_empty() {
        bail!("forward requires an IMAP account to fetch the source message");
    }
    let parent = {
        let mut client = imap::connect(creds)
            .await
            .context("failed to connect to IMAP")?;
        imap::fetch_message(&mut client, folder, uid)
            .await
            .context("failed to fetch source message")?
            .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?
    };
    let mut attachment_snapshots = if include_attachments {
        snapshot_source_attachments(creds, uid, folder, &parent.attachments).await?
    } else {
        Vec::new()
    };
    attachment_snapshots.extend(snapshot_attachments(attach_paths)?);
    let attachments = decode_attachments(&attachment_snapshots)?;

    let spec = ContextualDraftSpec {
        kind: DraftKind::Forward,
        source_folder: folder.to_string(),
        source_uid: uid,
        source_message_id: parent.message_id.clone(),
        to: to.unwrap_or("").to_string(),
        cc: None,
        bcc: None,
        subject: compose::prefix_forward_subject(&parent.subject),
        // Forwarding does not thread as a reply.
        in_reply_to: None,
        references: Vec::new(),
        agent_text: body.unwrap_or("").to_string(),
        agent_html: html.map(str::to_string),
        signature,
        context: compose::build_forward_context(&parent),
        preview_source: compose::message_preview_source(&parent),
        attachment_snapshots,
        attachments,
        attachments_forwarded: include_attachments,
    };
    create_contextual_draft(db, creds, spec).await
}

/// Modify the agent-authored part of a contextual draft, preserving the quote/
/// forward block and threading. Shared by the CLI and MCP surfaces.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn modify_draft(
    db: &Database,
    creds: &AccountWithCredentials,
    id: &str,
    body: Option<&str>,
    html: Option<&str>,
    to: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    subject: Option<&str>,
    add_signature: Option<bool>,
    attach_paths: &[String],
    remove_attachments: &[String],
    clear_attachments: bool,
) -> Result<Draft> {
    let draft = db
        .get_draft(id)
        .context("failed to get draft")?
        .ok_or_else(|| anyhow::anyhow!("draft not found: {id}"))?;
    if !draft.status.is_editable() {
        bail!(
            "draft {id} is not editable (status: {})",
            draft.status.as_str()
        );
    }
    // Bind the credential context to the draft's account before any local or
    // provider mutation.
    ensure_draft_account_binding(
        &draft.id,
        &draft.account_id,
        &creds.account.id,
        &creds.account.username,
    )?;

    let meta = draft
        .metadata
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    let context = context_from_metadata(&meta);

    // Agent body: override or keep prior authored content.
    let agent_text = body
        .map(str::to_string)
        .or_else(|| {
            meta.get("agent_body_text")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let agent_html = html.map(str::to_string).or_else(|| {
        meta.get("agent_body_html")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    });

    // Signature: explicit flag, else preserve prior applied state.
    let signature = add_signature.unwrap_or_else(|| {
        meta.get("signature_applied")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    });
    let (sig_text, sig_html) = if signature {
        (
            creds.account.signature_text.as_deref(),
            creds.account.signature_html.as_deref(),
        )
    } else {
        (None, None)
    };

    let assembled = compose::assemble_body(
        &agent_text,
        agent_html.as_deref(),
        sig_text,
        sig_html,
        signature,
        &context,
    );

    // Preserved threading + Message-ID.
    let (meta_in_reply_to, meta_references, _) = threading_from_metadata(Some(&meta));
    let new_to = to
        .map(str::to_string)
        .unwrap_or_else(|| draft.to_addr.clone());
    let new_cc = cc.map(str::to_string).or_else(|| draft.cc_addr.clone());
    let new_subject = subject
        .map(str::to_string)
        .or_else(|| draft.subject.clone())
        .unwrap_or_default();
    let mut attachment_snapshots = if clear_attachments {
        Vec::new()
    } else {
        draft.attachments.clone()
    };
    if !remove_attachments.is_empty() {
        attachment_snapshots.retain(|entry| {
            let filename = entry.get("filename").and_then(|v| v.as_str()).unwrap_or("");
            !remove_attachments.iter().any(|name| name == filename)
        });
    }
    if !attach_paths.is_empty() {
        attachment_snapshots.extend(snapshot_attachments(attach_paths)?);
    }
    let attachments =
        decode_attachments(&attachment_snapshots).context("failed to decode draft attachments")?;

    let from = account_from_header(creds);
    let (rfc822, message_id_hdr) = build_rfc822_full(
        &from,
        &new_to,
        &new_subject,
        Some(&assembled.text),
        assembled.html.as_deref(),
        new_cc.as_deref(),
        meta_in_reply_to.as_deref(),
        &meta_references,
        draft.message_id.as_deref(),
        &attachments,
    )?;

    // ── Exclusive sync lease BEFORE any mutation (local or provider) ──
    // The `syncing` claim (owner-token lease) is acquired first: the send
    // sweep cannot claim the row for the whole modify, no other actor can
    // mutate it, no partially-updated draft is ever claimable, and a crash
    // leaves the row inert as `syncing`.
    let claim = db
        .claim_draft_for_sync(id, draft.revision)
        .context("failed to claim draft for modify")?;
    let Some(claim) = claim else {
        bail!(
            "draft {id} was modified, claimed for sending, or changed state concurrently — \
             re-check with `envelope draft show {id}` and retry"
        );
    };

    // Everything below runs under the lease; on any failure release it back
    // to the prior status so the draft is never stranded by a soft error.
    let result = modify_claimed_draft(
        db,
        creds,
        id,
        &draft,
        &claim.token,
        &meta,
        &new_to,
        new_cc.as_deref(),
        bcc.or(draft.bcc_addr.as_deref()),
        &new_subject,
        &assembled,
        &agent_text,
        agent_html.as_deref(),
        &attachment_snapshots,
        &rfc822,
        &message_id_hdr,
    )
    .await;
    match db.release_syncing_draft(id, &claim.token, claim.prior_status.clone()) {
        Ok(true) => {}
        Ok(false) => warn!("draft {id}: sync lease release matched no owned `syncing` row"),
        Err(e) => {
            warn!("draft {id}: sync lease release failed: {e} — draft stays inert as `syncing`")
        }
    }
    result?;

    db.get_draft(id)
        .context("failed to reload draft")?
        .ok_or_else(|| anyhow::anyhow!("draft vanished after edit: {id}"))
}

/// The leased section of [`modify_draft`]: one atomic local edit, then the
/// provider replace with the owner token rechecked before each side effect.
#[allow(clippy::too_many_arguments)]
async fn modify_claimed_draft(
    db: &Database,
    creds: &AccountWithCredentials,
    id: &str,
    pre_edit: &Draft,
    token: &str,
    meta: &serde_json::Value,
    new_to: &str,
    new_cc: Option<&str>,
    new_bcc: Option<&str>,
    new_subject: &str,
    assembled: &compose::AssembledBody,
    agent_text: &str,
    agent_html: Option<&str>,
    attachment_snapshots: &[serde_json::Value],
    rfc822: &[u8],
    message_id_hdr: &str,
) -> Result<()> {
    // ── One atomic local edit under the lease ──
    // Content + recipients + attachments + metadata land in a single UPDATE
    // statement conditioned on the owner token: either the whole edit is
    // visible or none of it, and only to us — the row stays `syncing`.
    let mut edited_meta = meta.clone();
    if let Some(obj) = edited_meta.as_object_mut() {
        obj.insert("agent_body_text".into(), serde_json::json!(agent_text));
        obj.insert("agent_body_html".into(), serde_json::json!(agent_html));
        obj.insert(
            "signature_applied".into(),
            serde_json::json!(assembled.signature_applied),
        );
    }
    db.apply_synced_draft_edit(
        id,
        token,
        new_to,
        new_cc,
        new_bcc,
        new_subject,
        &assembled.text,
        assembled.html.as_deref(),
        attachment_snapshots,
        &edited_meta,
    )
    .context("failed to apply draft edit")?;

    if creds.account.imap_host.is_empty() {
        db.finalize_synced_draft_bookkeeping(
            id,
            token,
            None,
            &serde_json::json!({
                "imap_synced": false,
                "imap_folder": null,
                "local_only": true,
            }),
        )
        .context("failed to record storage state")?;
        return Ok(());
    }

    // ── Provider replace: delete old copy FIRST, exact + unique ──
    // The replacement APPEND reuses the same Message-ID, so before-the-append
    // is the only unambiguous window. If the old copy exists but cannot be
    // confirmably removed (unresolvable identity, ambiguous match, or delete
    // failure), the APPEND is SKIPPED — never a duplicate provider copy with
    // the same Message-ID. The local edit stands; the stale provider copy is
    // removed later by post-send exact cleanup, and storage metadata records
    // the actionable state.
    let provider_copy_expected = provider_copy_may_exist(pre_edit);
    let mut old_copy_cleared = !provider_copy_expected;
    if provider_copy_expected {
        use envelope_email_transport::draft_cleanup::{
            ProviderDraftCleanup, delete_provider_draft_exact, resolve_draft_cleanup_target,
        };
        // Recheck lease ownership immediately before the destructive delete.
        if !db.holds_sync_claim(id, token).unwrap_or(false) {
            bail!("draft {id}: sync lease lost before provider replace — aborting");
        }
        match resolve_draft_cleanup_target(db, pre_edit) {
            Ok(target) => match imap::connect(creds).await {
                Ok(mut client) => match delete_provider_draft_exact(&mut client, &target).await {
                    Ok(ProviderDraftCleanup::Deleted { uid }) => {
                        old_copy_cleared = true;
                        info!(
                            "draft {id}: removed stale provider copy (UID {uid} in {})",
                            target.folder
                        );
                    }
                    Ok(ProviderDraftCleanup::Skipped(reason)) => {
                        warn!(
                            "draft {id}: stale provider copy in {} not removed ({reason}) — \
                             replacement APPEND skipped to avoid a duplicate Message-ID",
                            target.folder
                        );
                    }
                    Err(e) => {
                        warn!(
                            "draft {id}: stale provider copy removal failed ({e}) — \
                             replacement APPEND skipped to avoid a duplicate Message-ID"
                        );
                    }
                },
                Err(e) => {
                    warn!(
                        "draft {id}: IMAP connect for provider re-sync failed ({e}) — \
                         replacement APPEND skipped"
                    );
                }
            },
            Err(reason) => {
                warn!(
                    "draft {id}: stale provider copy identity unresolvable ({reason}) — \
                     replacement APPEND skipped to avoid a duplicate Message-ID"
                );
            }
        }
    }

    let (imap_synced, imap_folder, new_uid) = if old_copy_cleared {
        // Recheck lease ownership immediately before the APPEND side effect.
        if !db.holds_sync_claim(id, token).unwrap_or(false) {
            bail!("draft {id}: sync lease lost before provider APPEND — aborting");
        }
        append_draft_best_effort(db, creds, rfc822, message_id_hdr).await
    } else {
        (false, String::new(), None)
    };

    let storage = if old_copy_cleared {
        serde_json::json!({
            "imap_synced": imap_synced,
            "imap_folder": if imap_synced { Some(imap_folder.clone()) } else { None },
            "local_only": !imap_synced,
        })
    } else {
        serde_json::json!({
            "imap_synced": false,
            "imap_folder": null,
            "local_only": true,
            "sync_status_reason": "stale_provider_copy_not_replaced",
        })
    };
    db.finalize_synced_draft_bookkeeping(id, token, new_uid, &storage)
        .context("failed to record storage state")?;
    Ok(())
}

/// Build and print (or pretty-print) the consistent contextual-draft envelope.
/// Build and print (or pretty-print) the consistent contextual-draft envelope.
fn emit_draft_envelope(draft: &Draft, json: bool) {
    if json {
        println!("{}", draft_envelope_json(draft));
        return;
    }
    let meta = draft
        .metadata
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    let kind = meta
        .get("draft_kind")
        .and_then(|v| v.as_str())
        .unwrap_or("draft");
    println!("Draft ({kind}) created: {}", draft.id);
    println!("  Status:  {}", draft_display_status(draft));
    println!("  To:      {}", draft.to_addr);
    if let Some(ref s) = draft.subject {
        println!("  Subject: {s}");
    }
    if let Some(ref cc) = draft.cc_addr {
        println!("  CC:      {cc}");
    }
    if !draft.attachments.is_empty() {
        println!("  Attachments: {}", draft.attachments.len());
        for a in attachment_summaries(&draft.attachments) {
            println!(
                "    - {} ({} bytes, {})",
                a["filename"].as_str().unwrap_or("attachment"),
                a["size"].as_u64().unwrap_or(0),
                a["content_type"]
                    .as_str()
                    .unwrap_or("application/octet-stream"),
            );
        }
    }
    if let Some(preview) = meta.get("preview_text").and_then(|v| v.as_str()) {
        if !preview.is_empty() {
            println!("  Quote preview: {preview}");
        }
    }
    println!(
        "  Review:  {}",
        draft_dashboard_url(&draft.account_id, &draft.id)
    );
    if draft.imap_uid.is_some() {
        println!("  IMAP:    synced (UID {})", draft.imap_uid.unwrap());
    } else {
        println!("  ⚠ IMAP:  saved locally only");
    }
}

/// The truthful persisted status for `draft show` / the draft envelope, derived
/// from the durable `DraftStatus` (and `send_after` for a queued draft).
///
/// Previously the envelope hard-coded `"drafted"`, so `draft show` reported a
/// SENT or PENDING-REVIEW draft as an ordinary local draft (real evidence). This
/// distinguishes at least `sent`, `pending_review`, `queued` (a `draft` row with a
/// `send_after` schedule), and ordinary `drafted`, and never collapses them.
fn draft_display_status(draft: &Draft) -> &'static str {
    use envelope_email_store::DraftStatus;
    match draft.status {
        DraftStatus::Sent => "sent",
        DraftStatus::PendingReview => "pending_review",
        DraftStatus::Blocked => "blocked",
        DraftStatus::Sending => "sending",
        DraftStatus::Syncing => "syncing",
        DraftStatus::DeliveryUncertain => "delivery_uncertain",
        DraftStatus::Discarded => "discarded",
        DraftStatus::Draft => {
            if draft.send_after.is_some() {
                "queued"
            } else {
                "drafted"
            }
        }
    }
}

/// Render the scope-defined draft envelope JSON from a stored draft + metadata.
pub(crate) fn draft_envelope_json(draft: &Draft) -> serde_json::Value {
    let meta = draft
        .metadata
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    let get = |k: &str| meta.get(k).cloned().unwrap_or(serde_json::Value::Null);
    let ctx = meta.get("context");
    let quote_included = ctx
        .and_then(|c| c.get("included"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let storage = meta.get("storage");
    let imap_synced = storage
        .and_then(|s| s.get("imap_synced"))
        .and_then(|v| v.as_bool())
        .unwrap_or(draft.imap_uid.is_some());
    let dashboard_path = draft_dashboard_path(&draft.account_id, &draft.id);
    let dashboard_url = draft_dashboard_url(&draft.account_id, &draft.id);

    // Threading comes from metadata-or-column, so a reply that only ever set
    // the column still reports its parent instead of posing as a new message.
    let (resolved_in_reply_to, resolved_references, _) = threading_for_draft(draft);
    let draft_kind = meta.get("draft_kind").and_then(|v| v.as_str()).unwrap_or(
        if resolved_in_reply_to.is_some() {
            "reply"
        } else {
            "new"
        },
    );

    serde_json::json!({
        "status": draft_display_status(draft),
        "draft_id": draft.id,
        "account_id": draft.account_id,
        "draft_kind": draft_kind,
        "source": get("source"),
        "fields": {
            "to": draft.to_addr,
            "cc": draft.cc_addr,
            "bcc": draft.bcc_addr,
            "subject": draft.subject,
            "in_reply_to": resolved_in_reply_to,
            "references": serde_json::json!(resolved_references),
            "message_id": draft.message_id,
        },
        "content": {
            "agent_body_text": get("agent_body_text"),
            "agent_body_html": get("agent_body_html"),
            "signature_applied": meta.get("signature_applied").and_then(|v| v.as_bool()).unwrap_or(false),
            "quote_included": quote_included,
            "quote_format": get("quote_format"),
            "preview_text": get("preview_text"),
            "preview_truncated": meta.get("preview_truncated").and_then(|v| v.as_bool()).unwrap_or(false),
            "preview_word_limit": meta.get("preview_word_limit").cloned().unwrap_or_else(|| serde_json::json!(DEFAULT_PREVIEW_WORD_LIMIT)),
            "attachments_forwarded": meta.get("attachments_forwarded").and_then(|v| v.as_bool()).unwrap_or(false),
            "full_content_preserved": true,
        },
        "attachments": attachment_summaries(&draft.attachments),
        "storage": {
            "imap_synced": imap_synced,
            "imap_folder": storage.and_then(|s| s.get("imap_folder")).cloned().unwrap_or(serde_json::Value::Null),
            "imap_uid": draft.imap_uid,
            "local_only": !imap_synced,
            "sync_status_reason": storage.and_then(|s| s.get("sync_status_reason")).cloned().unwrap_or(serde_json::Value::Null),
        },
        "dashboard_path": dashboard_path,
        "dashboard_url": dashboard_url,
        "ui": ui::draft_ui(&draft.account_id, &draft.id),
    })
}

/// `envelope draft reply <uid>` — create a contextual reply draft.
#[tokio::main]
#[allow(clippy::too_many_arguments)]
pub async fn run_reply(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
    reply_all: bool,
    body: Option<&str>,
    html: Option<&str>,
    signature: bool,
    attach_paths: &[String],
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let draft = create_reply_draft(
        &db,
        &creds,
        uid,
        folder,
        reply_all,
        body,
        html,
        signature,
        attach_paths,
    )
    .await?;
    emit_draft_envelope(&draft, json);
    Ok(())
}

/// `envelope draft forward <uid>` — create a contextual forward draft.
///
/// No reply threading headers are set by default; attachments are described in
/// the preview but not re-attached (MVP).
#[tokio::main]
#[allow(clippy::too_many_arguments)]
pub async fn run_forward(
    uid: u32,
    folder: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
    to: Option<&str>,
    body: Option<&str>,
    html: Option<&str>,
    signature: bool,
    attach_paths: &[String],
    include_attachments: bool,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let draft = create_forward_draft(
        &db,
        &creds,
        uid,
        folder,
        to,
        body,
        html,
        signature,
        attach_paths,
        include_attachments,
    )
    .await?;
    emit_draft_envelope(&draft, json);
    Ok(())
}

/// `envelope draft edit <id>` — modify the agent-authored part of a draft.
///
/// The preserved quote/forward block is recombined automatically; the agent
/// only replaces its authored body (and may override recipient fields).
#[tokio::main]
#[allow(clippy::too_many_arguments)]
pub async fn run_edit(
    id: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
    body: Option<&str>,
    html: Option<&str>,
    to: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    subject: Option<&str>,
    add_signature: Option<bool>,
    attach_paths: &[String],
    remove_attachments: &[String],
    clear_attachments: bool,
) -> Result<()> {
    let (db, creds) = setup_credentials(account, backend)?;
    let draft = modify_draft(
        &db,
        &creds,
        id,
        body,
        html,
        to,
        cc,
        bcc,
        subject,
        add_signature,
        attach_paths,
        remove_attachments,
        clear_attachments,
    )
    .await?;
    emit_draft_envelope(&draft, json);
    Ok(())
}

/// `envelope draft show <id>` — print the draft envelope (metadata + abridged
/// preview). Read-only; no IMAP access.
pub fn run_show(id: &str, json: bool) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let draft = db
        .get_draft(id)
        .context("failed to get draft")?
        .ok_or_else(|| anyhow::anyhow!("draft not found: {id}"))?;
    if json {
        println!("{}", draft_envelope_json(&draft));
    } else {
        emit_draft_envelope(&draft, false);
    }
    Ok(())
}

// ─── draft list ──────────────────────────────────────────────────────────

#[tokio::main]
pub async fn run_list(account: Option<&str>, json: bool, backend: CredentialBackend) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let passphrase =
        credential_store::get_or_create_passphrase(backend).context("credential store error")?;
    let acct = resolve_account(&db, account)?;

    // Check if account has IMAP
    if acct.imap_host.is_empty() {
        // Send-only account: fall back to local SQLite
        return run_list_local(&db, &acct.id, json);
    }

    let creds = db
        .get_account_with_credentials(&acct.id, &passphrase)
        .context("failed to decrypt credentials")?;

    // Try IMAP first — that's the source of truth
    match imap::connect(&creds).await {
        Ok(mut client) => {
            let drafts_folder = detect_drafts_folder(&mut client, &db, &acct.id)
                .await
                .map_err(|e| anyhow::anyhow!("drafts folder detection failed: {e}"))?;
            let drafts_folder = match drafts_folder {
                Some(f) => f,
                None => {
                    warn!(
                        "no drafts folder detected for {}, falling back to local",
                        acct.username
                    );
                    return run_list_local(&db, &acct.id, json);
                }
            };

            // Fetch all messages from the Drafts folder
            let summaries = imap::fetch_inbox(&mut client, &drafts_folder, 100)
                .await
                .map_err(|e| anyhow::anyhow!("failed to fetch drafts from IMAP: {e}"))?;

            if json {
                let items: Vec<serde_json::Value> = summaries
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "uid": s.uid,
                            "from": s.from_addr,
                            "to": s.to_addr,
                            "subject": s.subject,
                            "date": s.date,
                            "size": s.size,
                            "message_id": s.message_id,
                            "flags": s.flags,
                            "source": "imap",
                            "folder": drafts_folder,
                            "ui": ui::message_or_draft_ui(&db, &acct.id, s.uid, &drafts_folder),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                if summaries.is_empty() {
                    println!("No drafts for {} (IMAP: {})", acct.username, drafts_folder);
                    return Ok(());
                }

                println!("{:<8}  {:<30}  {:<40}  {}", "UID", "TO", "SUBJECT", "DATE");
                println!("{}", "-".repeat(90));
                for s in &summaries {
                    let subject_display = if s.subject.len() > 38 {
                        format!("{}...", &s.subject[..38])
                    } else {
                        s.subject.clone()
                    };
                    let to_display = if s.to_addr.len() > 28 {
                        format!("{}...", &s.to_addr[..28])
                    } else {
                        s.to_addr.clone()
                    };
                    let date_display = s.date.as_deref().unwrap_or("-");
                    println!(
                        "{:<8}  {:<30}  {:<40}  {}",
                        s.uid, to_display, subject_display, date_display,
                    );
                }
                println!("\n{} draft(s) in {} (IMAP)", summaries.len(), drafts_folder);
            }
            Ok(())
        }
        Err(e) => {
            warn!("IMAP connect failed, falling back to local: {e}");
            run_list_local(&db, &acct.id, json)
        }
    }
}

/// Fallback: list drafts from local SQLite when IMAP is unavailable.
fn run_list_local(db: &Database, account_id: &str, json: bool) -> Result<()> {
    let drafts = db
        .list_drafts(account_id, Some("draft"), 100, 0)
        .context("failed to list drafts")?;

    if json {
        let items: Vec<serde_json::Value> = drafts
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "to": d.to_addr,
                    "subject": d.subject,
                    "updated_at": d.updated_at,
                    "imap_uid": d.imap_uid,
                    "source": "local",
                    "dashboard_path": draft_dashboard_path(&d.account_id, &d.id),
                    "dashboard_url": draft_dashboard_url(&d.account_id, &d.id),
                    "review_url": draft_dashboard_url(&d.account_id, &d.id),
                    "ui": ui::draft_ui(account_id, &d.id),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        if drafts.is_empty() {
            println!("No local drafts");
            return Ok(());
        }

        println!(
            "{:<36}  {:<30}  {:<40}  {}",
            "ID", "TO", "SUBJECT", "UPDATED"
        );
        println!("{}", "-".repeat(110));
        for d in &drafts {
            let subject = d.subject.as_deref().unwrap_or("-");
            let subject_display = if subject.len() > 38 {
                format!("{}...", &subject[..38])
            } else {
                subject.to_string()
            };
            let to_display = if d.to_addr.len() > 28 {
                format!("{}...", &d.to_addr[..28])
            } else {
                d.to_addr.clone()
            };
            println!(
                "{:<36}  {:<30}  {:<40}  {}",
                d.id, to_display, subject_display, d.updated_at,
            );
        }
        println!(
            "\n{} draft(s) (local only — IMAP unavailable)",
            drafts.len()
        );
    }

    Ok(())
}

// ─── draft create ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
#[tokio::main]
pub async fn run_create(
    to: &str,
    subject: Option<&str>,
    body: Option<&str>,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
    from: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    in_reply_to: Option<&str>,
    attach_paths: &[String],
    confirm_new_re_subject: bool,
) -> Result<()> {
    check_new_re_subject_guard(subject, in_reply_to.is_some(), confirm_new_re_subject, json)?;

    let (db, creds) = setup_credentials(account, backend)?;

    // Snapshot attachment bytes now so review/send preserve them even if the
    // source files later change. Fail explicitly if a file is unreadable rather
    // than creating a draft with a silently-missing attachment.
    let attachment_snapshots = snapshot_attachments(attach_paths)?;
    let attachments = decode_attachments(&attachment_snapshots)?;

    // Build RFC822 message for IMAP APPEND
    let from_addr = from
        .map(str::to_string)
        .unwrap_or_else(|| account_from_header(&creds));

    let (rfc822, message_id) = build_rfc822_draft(
        &from_addr,
        to,
        subject,
        body,
        cc,
        bcc,
        in_reply_to,
        &attachments,
    )?;

    // Check if this is a send-only account (no IMAP)
    // IMAP-first and mandatory: a draft's real home is the account's Drafts
    // folder — what Mail.app and every other IMAP client display. A local-only
    // SQLite record is invisible to those clients, so we never silently fall
    // back to it; append_draft_required fails loud on any IMAP/folder error.
    let (drafts_folder_name, imap_uid) =
        append_draft_required(&db, &creds, &rfc822, &message_id).await?;

    // ── Local SQLite record: secondary cache/reference ──
    let draft = db
        .create_draft(
            &creds.account.id,
            to,
            subject,
            body,
            None, // html_content
            in_reply_to,
            cc,
            bcc,
            Some("cli"),
        )
        .context("failed to create local draft record")?;

    // Store the IMAP UID in the local DB if we got one
    if let Some(uid) = imap_uid {
        if let Err(e) = db.update_draft_imap_uid(&draft.id, uid) {
            warn!("failed to store IMAP UID in local DB: {e}");
        }
    }

    // Store the message_id in local DB
    if !message_id.is_empty() {
        let _ = db.mark_draft_message_id(&draft.id, &message_id);
    }

    // Persist the (non-secret metadata + base64 payload) attachment snapshots so
    // a later `draft send` re-includes them rather than silently dropping.
    if !attachment_snapshots.is_empty() {
        db.update_draft_attachments(&draft.id, &attachment_snapshots)
            .context("failed to persist draft attachments")?;
    }
    let attachment_summary = attachment_summaries(&attachment_snapshots);

    let dashboard_path = draft_dashboard_path(&creds.account.id, &draft.id);
    let dashboard_url = draft_dashboard_url(&creds.account.id, &draft.id);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": draft.id,
                "to": draft.to_addr,
                "subject": draft.subject,
                "cc": cc,
                "bcc": bcc,
                "in_reply_to": in_reply_to,
                "attachments": attachment_summary,
                "imap_synced": true,
                "imap_uid": imap_uid,
                "imap_folder": drafts_folder_name,
                "local_only": false,
                "sync_status_reason": serde_json::Value::Null,
                "dashboard_path": dashboard_path,
                "dashboard_url": dashboard_url,
                "review_url": dashboard_url,
                "metadata": {
                    "dashboard_path": dashboard_path,
                    "dashboard_url": dashboard_url,
                    "review_url": dashboard_url,
                },
                "warning": serde_json::Value::Null,
                "ui": ui::draft_ui(&creds.account.id, &draft.id),
            })
        );
    } else {
        println!("Draft created: {}", draft.id);
        println!("  To:      {}", draft.to_addr);
        if let Some(ref s) = draft.subject {
            println!("  Subject: {s}");
        }
        if let Some(c) = cc {
            println!("  CC:      {c}");
        }
        if !attachment_summary.is_empty() {
            println!("  Attachments: {}", attachment_summary.len());
            for a in &attachment_summary {
                println!(
                    "    - {} ({} bytes, {})",
                    a["filename"].as_str().unwrap_or("attachment"),
                    a["size"].as_u64().unwrap_or(0),
                    a["content_type"]
                        .as_str()
                        .unwrap_or("application/octet-stream"),
                );
            }
        }
        println!("  Review:  {dashboard_url}");
        if let Some(uid) = imap_uid {
            println!("  IMAP:    saved to {drafts_folder_name} (UID {uid})");
        } else {
            println!("  IMAP:    saved to {drafts_folder_name} (UID pending)");
        }
    }

    Ok(())
}

// ─── draft send ──────────────────────────────────────────────────────────

#[tokio::main]
pub async fn run_send(
    id: &str,
    account: Option<&str>,
    attr: &[String],
    json: bool,
    backend: CredentialBackend,
    cooldown_seconds: Option<i64>,
    send_now: bool,
    confirm_send_now: bool,
) -> Result<()> {
    use envelope_email_transport::outbound::{
        IMMEDIATE_SEND_CONFIRM_CODE, SendDisposition, resolve_cooldown_seconds, resolve_disposition,
    };

    // ── Attribution precheck (before ANY side effect, incl. queueing) ──
    //
    // Capture the EXACT validated revision + resolution here so the queue CAS
    // binds to the revision the declaration was validated against — never a
    // reloaded, possibly concurrently-edited newer revision. The db is scoped so
    // no connection is held across the later async send.
    let declared: Vec<String> = attr.to_vec();
    let precheck = {
        let db = Database::open_default().context("failed to open database")?;
        let precheck = precheck_draft(&db, id, SendSurface::Cli, &declared, None)?;
        if let Some(outcome) = &precheck.refusal {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": outcome.status_str(),
                        "draft_id": id,
                        "error": outcome.error_json(),
                    })
                );
            }
            anyhow::bail!("{}", outcome.reason_string());
        }
        precheck
    };

    // ── Default actual-send cooldown (outbox queueing) ──
    // `draft send` queues by default: it sets send_after on the draft so the
    // scheduled-send sweep transmits it later (after the Governor gate permits
    // it). Immediate transmission requires an explicit, confirmed bypass.
    let cooldown = resolve_cooldown_seconds(cooldown_seconds);
    match resolve_disposition(cooldown, send_now, confirm_send_now) {
        SendDisposition::NeedsConfirmation => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "denied",
                        "draft_id": id,
                        "error": {
                            "code": IMMEDIATE_SEND_CONFIRM_CODE,
                            "reason": "immediate send bypasses the outbox cooldown; pass --send-now together with --confirm-send-now",
                        },
                    })
                );
            }
            anyhow::bail!(
                "immediate send requires confirmation: pass --send-now together with --confirm-send-now"
            );
        }
        SendDisposition::Queue {
            cooldown_seconds: cd,
        } => {
            let db = Database::open_default().context("failed to open database")?;
            let send_at = (chrono::Utc::now() + chrono::Duration::seconds(cd))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            // One atomic CAS bound to the EXACT revision the declaration was
            // validated against at precheck (never a reloaded revision): a
            // concurrent material edit conflicts rather than binding a stale
            // declaration or leaving a partial schedule; a re-queued
            // pending_review draft transitions to the due `draft` status.
            queue_bot_draft_for_send(&db, id, precheck.revision, &send_at, &declared)?;
            // The additive success block is built from the SAME validated
            // resolution — no re-resolve that could observe edited content.
            let queued_attribution =
                envelope_email_transport::attribution_persist::success_attribution_block(
                    &precheck.resolution,
                    None,
                    None,
                    true,
                );
            // Catalog event: a send was queued into the outbox. Payload carries
            // only the transition metadata — no recipients, no body.
            let _ = db.emit_catalog_event(
                &precheck.account_id,
                envelope_email_store::event_catalog::SEND_QUEUED,
                Some(serde_json::json!({
                    "draft_id": id,
                    "send_after": send_at,
                    "cooldown_seconds": cd,
                })),
                None,
            );
            if json {
                println!(
                    "{}",
                    crate::commands::contract::send_body::draft_scheduled(
                        false,
                        id,
                        &send_at,
                        cd,
                        queued_attribution,
                        ui::draft_ui(&precheck.account_id, id),
                    )
                );
            } else {
                println!(
                    "Queued draft {id} for send after {cd}s cooldown (at {send_at}). \
                     Real send happens via the scheduled-send sweep, after the Governor gate."
                );
            }
            return Ok(());
        }
        SendDisposition::Immediate => {}
    }

    // Catalog event: the operator approved this draft for immediate send. This
    // is the human-confirmed transition (--send-now --confirm-send-now).
    if let Ok(db) = Database::open_default()
        && let Ok(Some(draft)) = db.get_draft(id)
    {
        let _ = db.emit_catalog_event(
            &draft.account_id,
            envelope_email_store::event_catalog::DRAFT_APPROVED,
            Some(serde_json::json!({ "draft_id": id })),
            None,
        );
    }

    let outcome = send_existing_draft(id, account, backend, SendSurface::Cli, &declared).await?;
    if json {
        println!("{}", outcome.json);
    } else {
        println!("Draft {id} sent to {}", outcome.to_addr);
        println!("Subject: {}", outcome.subject);
        println!("Message-ID: {}", outcome.message_id);
        match (outcome.sent_folder.as_deref(), outcome.sent_uid) {
            (Some(folder), Some(uid)) => {
                println!("Sent UID: {uid} ({folder})");
                if let Some(ref url) = outcome.sent_url {
                    println!("Sent URL: {url}");
                }
            }
            (Some(folder), None) => println!(
                "Sent UID: unavailable in {folder} ({})",
                outcome.lookup_status
            ),
            (None, None) => println!("Sent UID: unavailable ({})", outcome.lookup_status),
            (None, Some(uid)) => println!("Sent UID: {uid}"),
        }
    }
    Ok(())
}

/// Structured result of sending an existing draft. Carries both the JSON
/// contract payload and the discrete fields the human CLI output needs, so the
/// silent send primitive can serve the CLI, MCP, and any other surface without
/// printing to stdout (which would corrupt the MCP stdio transport).
/// True when a provider draft copy may exist for this draft — from durable
/// storage provenance OR a recorded UID, never UID alone: APPEND can succeed
/// while the post-APPEND UID lookup returns `None`, leaving `imap_uid` NULL
/// with a real copy on the server (`metadata.storage.imap_synced == true`).
/// Only genuinely local-only drafts (`imap_synced` false/absent, no UID) may
/// APPEND a fresh copy without clearing an old one first.
fn provider_copy_may_exist(draft: &Draft) -> bool {
    if draft.imap_uid.is_some() {
        return true;
    }
    draft
        .metadata
        .as_ref()
        .and_then(|m| m.get("storage"))
        .and_then(|s| s.get("imap_synced"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Refuse any modify/send when the resolved credential context does not
/// belong to the draft's account. Checked BEFORE any claim, local mutation,
/// or provider/network side effect — a mismatched `--account` must never
/// touch another account's mailbox with the wrong credentials.
fn ensure_draft_account_binding(
    draft_id: &str,
    draft_account_id: &str,
    creds_account_id: &str,
    creds_username: &str,
) -> Result<()> {
    if draft_account_id != creds_account_id {
        bail!(
            "draft {draft_id} belongs to account {draft_account_id}, but the resolved \
             credentials are for {creds_username} ({creds_account_id}) — refusing before \
             any mailbox or network side effect. Drop --account or pass the draft's own account."
        );
    }
    Ok(())
}

/// Releases an immediate-send `sending` claim back to `draft` on early exit
/// (any pre-SMTP failure, including panics). Disarmed after SMTP acceptance —
/// from that point the claim may only be left via `mark_draft_sent` or an
/// explicit anti-duplicate park.
struct SendClaimGuard<'a> {
    db: &'a Database,
    draft_id: String,
    /// Opaque owner lease token from the claim; release requires it.
    lease: String,
    armed: bool,
}

impl<'a> SendClaimGuard<'a> {
    fn new(db: &'a Database, draft_id: &str, lease: String) -> Self {
        Self {
            db,
            draft_id: draft_id.to_string(),
            lease,
            armed: true,
        }
    }

    /// SMTP was accepted: the claim must no longer be released to `draft`.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SendClaimGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            match self.db.release_sending_draft(
                &self.draft_id,
                &self.lease,
                envelope_email_store::DraftStatus::Draft,
            ) {
                Ok(true) => {}
                Ok(false) => warn!(
                    "draft {}: send-claim release matched no `sending` row",
                    self.draft_id
                ),
                Err(e) => warn!(
                    "draft {}: send-claim release failed: {e} — draft stays inert as `sending`",
                    self.draft_id
                ),
            }
        }
    }
}

pub(crate) struct SentDraftOutcome {
    pub json: serde_json::Value,
    pub to_addr: String,
    pub subject: String,
    pub message_id: String,
    pub sent_folder: Option<String>,
    pub sent_uid: Option<u32>,
    pub sent_url: Option<String>,
    pub lookup_status: &'static str,
}

/// Attribution precheck for a draft-send **before any side effect** (queueing or
/// SMTP). Loads the draft, derives its sanitized send context, and returns the
/// canonical refusal outcome for an unattributed/invalid request; `None` to
/// proceed. No Governor spawn on refusal.
/// The validated attribution precheck for a draft-send, produced BEFORE any
/// side effect (queueing or SMTP). It captures the exact state the queue CAS
/// must bind to, so a concurrent material edit conflicts instead of inheriting a
/// stale declaration.
pub(crate) struct DraftSendPrecheck {
    /// The row `revision` at validation time. The atomic queue CAS
    /// ([`queue_bot_draft_for_send`]) binds to exactly this value; a concurrent
    /// edit (revision moved) then conflicts rather than binding the validated
    /// declaration to newer, unvalidated content.
    pub revision: i64,
    /// The draft's account id, captured at validation so the queue path can emit
    /// its catalog/UI metadata without a second load.
    pub account_id: String,
    /// The resolved attribution (declared ∪ host-derived), for the additive
    /// success `attribution` block. Never a score/weight/threshold.
    pub resolution: envelope_email_transport::attribution::AttributionResolution,
    /// `Some` when the attribution precondition refuses the send before any side
    /// effect (unattributed/invalid on a bot surface); the refusal is already
    /// recorded in the audit log. `None` when the send may proceed.
    pub refusal: Option<envelope_email_transport::outbound::GovernorOutcome>,
}

/// Resolve and validate a draft-send's attribution BEFORE any side effect, and
/// return the exact revision + resolution the queue CAS must bind to.
///
/// This is a `Result`, not an `Option`: a load failure (missing draft/account,
/// DB error) is a real error propagated to the caller, never silently swallowed
/// into a "proceed" outcome. The returned `revision` is the row revision the
/// declaration was validated against; callers MUST pass it back into
/// [`queue_bot_draft_for_send`] rather than reloading — a reload could pick up a
/// concurrently-edited newer revision and bind the stale declaration to it.
pub(crate) fn precheck_draft(
    db: &Database,
    draft_id: &str,
    surface: SendSurface,
    declared: &[String],
    agent_id: Option<&str>,
) -> Result<DraftSendPrecheck> {
    let draft = db
        .get_draft(draft_id)
        .context("failed to load draft for attribution precheck")?
        .ok_or_else(|| anyhow::anyhow!("draft not found: {draft_id}"))?;
    let acct = db
        .get_account(&draft.account_id)
        .context("failed to load account for attribution precheck")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "account not found for draft {draft_id}: {}",
                draft.account_id
            )
        })?;
    let attachments = draft_attachment_stubs(&draft);
    let gov_req = super::governor_gate::governor_request(
        &draft.account_id,
        super::governor_gate::account_domain(&acct.username),
        draft.subject.as_deref().unwrap_or(""),
        &draft.to_addr,
        draft.cc_addr.as_deref(),
        draft.bcc_addr.as_deref(),
        surface,
        Some(draft_id),
        &attachments,
        draft.in_reply_to.is_some(),
        draft.text_content.as_deref(),
        draft.html_content.as_deref(),
        declared,
    );
    // `governor_request` always resolves attribution, so this is present; treat a
    // missing resolution as a hard error rather than silently proceeding.
    let resolution = gov_req.resolution.clone().ok_or_else(|| {
        anyhow::anyhow!("attribution resolution unavailable for draft {draft_id}")
    })?;
    let refusal =
        super::governor_gate::precheck_attribution(db, &draft.account_id, &gov_req, agent_id);
    Ok(DraftSendPrecheck {
        revision: draft.revision,
        account_id: draft.account_id.clone(),
        resolution,
        refusal,
    })
}

/// Body-free attachment stubs (filename + content_type only) for deriving
/// host attribution facts (`has_attachment`, `sensitive_attachment`, count)
/// without rehydrating the snapshotted bytes.
fn draft_attachment_stubs(
    draft: &envelope_email_store::Draft,
) -> Vec<envelope_email_transport::smtp::Attachment> {
    draft
        .attachments
        .iter()
        .map(|s| envelope_email_transport::smtp::Attachment {
            filename: s
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("attachment")
                .to_string(),
            content_type: s
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream")
                .to_string(),
            data: Vec::new(),
        })
        .collect()
}

/// Atomically queue a bot draft for scheduled sending: bind the bot's validated
/// declaration to `expected_revision`, set `send_after`, and transition the row
/// to the due `draft` status in ONE store compare-and-set (see
/// [`Database::queue_draft_for_send`]).
///
/// `expected_revision` is the revision the declaration was validated against. A
/// concurrent material edit (revision moved) fails with a conflict and leaves
/// nothing scheduled or persisted — a stale declaration is never bound to newer
/// content, and no partial schedule survives.
pub(crate) fn queue_bot_draft_for_send(
    db: &Database,
    draft_id: &str,
    expected_revision: i64,
    send_after: &str,
    declared: &[String],
) -> Result<()> {
    let attribution = envelope_email_transport::attribution_persist::PersistedDeclaration::new_bot(
        declared,
        expected_revision,
    )
    .to_value();
    db.queue_draft_for_send(draft_id, expected_revision, send_after, &attribution)
        .context("failed to atomically queue draft (declaration + schedule + due status)")
}

/// Send an already-created draft (by local UUID or IMAP UID) without printing
/// anything. This is the single source of truth for "send this draft": it sends
/// over SMTP, cleans up the IMAP Drafts copy, optionally appends to Sent, and —
/// critically — marks the local draft row as sent so the local DB can never be
/// left at `status=draft` with no `sent_at` after a successful send.
pub(crate) async fn send_existing_draft(
    id: &str,
    account: Option<&str>,
    backend: CredentialBackend,
    surface: SendSurface,
    declared: &[String],
) -> Result<SentDraftOutcome> {
    let db = Database::open_default().context("failed to open database")?;
    let passphrase =
        credential_store::get_or_create_passphrase(backend).context("credential store error")?;

    // `id` can be either a local draft UUID or an IMAP UID (numeric).
    let is_imap_uid = id.parse::<u32>().is_ok();
    let local_draft = db.get_draft(id).context("failed to get draft")?;

    // Resolve account
    let acct = match account {
        Some(a) => resolve_account(&db, Some(a))?,
        None => {
            if let Some(ref d) = local_draft {
                db.get_account(&d.account_id)
                    .context("database error")?
                    .ok_or_else(|| {
                        anyhow::anyhow!("account not found for draft: {}", d.account_id)
                    })?
            } else {
                let acct = db
                    .default_account()
                    .context("failed to query default account")?;
                acct.ok_or_else(|| {
                    anyhow::anyhow!(
                        "no --account specified and no default account. \
                         Use --account to specify which account this IMAP draft belongs to."
                    )
                })?
            }
        }
    };

    let creds = db
        .get_account_with_credentials(&acct.id, &passphrase)
        .context("failed to decrypt credentials")?;

    // Determine the IMAP UID to fetch the draft from
    let imap_uid: Option<u32> = if let Some(ref d) = local_draft {
        d.imap_uid
    } else if is_imap_uid {
        Some(id.parse::<u32>().unwrap())
    } else {
        None
    };

    // ── Resolve raw numeric IMAP ids to the local draft record (fail closed) ──
    // Every send rides the durable claim of a LOCAL draft. A bare IMAP UID is
    // resolved through the existing account+imap_uid mapping; if no local
    // record exists there is no revision to claim and no persisted cleanup
    // identity, so the send is refused with an import/review path instead of
    // an unclaimed compose-and-guess flow.
    let local_draft = match local_draft {
        Some(d) => Some(d),
        None if is_imap_uid => {
            let uid: u32 = id.parse().unwrap();
            match db
                .get_draft_by_imap_uid(&acct.id, uid)
                .context("failed to resolve IMAP draft to a local record")?
            {
                Some(d) => Some(d),
                None => bail!(
                    "IMAP draft UID {uid} in account {} has no local Envelope draft record \
                     and cannot be sent safely. Review it first (dashboard Drafts view or \
                     `envelope draft list --account {}`) or recreate it as an Envelope draft.",
                    acct.username,
                    acct.username
                ),
            }
        }
        None => None,
    };

    // ── Bind credentials to the draft's account (before any network work) ──
    if let Some(d) = &local_draft {
        ensure_draft_account_binding(&d.id, &d.account_id, &acct.id, &acct.username)?;
    }

    // ── Exclusive durable send claim ──
    // The same `sending` claim the scheduled sweep uses, acquired before any
    // Governor/SMTP work: an in-flight sweep claim, a provider sync, a
    // concurrent edit (stale revision), or any non-`draft` status loses here
    // instead of double-sending or transmitting a stale snapshot. The claim
    // returns an owner lease token: only its holder can mark-sent, park, or
    // release. The guard releases the claim back to `draft` on every pre-SMTP
    // failure; after SMTP acceptance the claim is left only via
    // `mark_draft_sent`.
    let mut claim_guard: Option<SendClaimGuard<'_>> = None;
    let local_draft = match local_draft {
        Some(d) => {
            let lease = match db
                .claim_draft_for_immediate_send(&d.id, d.revision)
                .context("failed to claim draft for sending")?
            {
                Some(lease) => lease,
                None => {
                    let status = db
                        .get_draft(&d.id)
                        .ok()
                        .flatten()
                        .map(|cur| cur.status.as_str().to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    bail!(
                        "draft {} is not sendable right now (status '{status}'): it may be \
                         mid-send, mid-sync, awaiting review, or just modified — re-check \
                         with `envelope draft show {}`",
                        d.id,
                        d.id
                    );
                }
            };
            claim_guard = Some(SendClaimGuard::new(&db, &d.id, lease));
            // Reload the claimed row: the authoritative send snapshot.
            let claimed = db
                .get_draft(&d.id)
                .context("failed to reload claimed draft")?
                .ok_or_else(|| anyhow::anyhow!("draft vanished after claim: {}", d.id))?;
            Some(claimed)
        }
        None => None,
    };

    // Threading (In-Reply-To / References) + preserved Message-ID from the
    // local draft metadata — preferred so reply headers survive the send.
    let (meta_in_reply_to, meta_references, _meta_message_id) = local_draft
        .as_ref()
        .map(threading_for_draft)
        .unwrap_or((None, Vec::new(), None));

    // ── Fetch draft content from IMAP (source of truth) ──
    let (
        to_addr,
        subject,
        text_body,
        html_body,
        cc_addr,
        bcc_addr,
        reply_to,
        in_reply_to,
        references,
    ) = if let Some(uid) = imap_uid {
        if acct.imap_host.is_empty() {
            if let Some(ref d) = local_draft {
                (
                    d.to_addr.clone(),
                    d.subject.clone().unwrap_or_default(),
                    d.text_content.clone(),
                    d.html_content.clone(),
                    d.cc_addr.clone(),
                    d.bcc_addr.clone(),
                    d.reply_to.clone(),
                    d.in_reply_to.clone().or(meta_in_reply_to.clone()),
                    meta_references.clone(),
                )
            } else {
                bail!("draft {id} not found locally and account has no IMAP");
            }
        } else {
            let mut client = imap::connect(&creds)
                .await
                .context("failed to connect to IMAP to fetch draft")?;

            let drafts_folder = detect_drafts_folder(&mut client, &db, &acct.id)
                .await
                .map_err(|e| anyhow::anyhow!("drafts folder detection failed: {e}"))?
                .unwrap_or_else(|| "Drafts".to_string());

            let msg = imap::fetch_message(&mut client, &drafts_folder, uid)
                .await
                .map_err(|e| anyhow::anyhow!("failed to fetch draft UID {uid} from IMAP: {e}"))?
                .ok_or_else(|| {
                    anyhow::anyhow!("draft UID {uid} not found in IMAP {drafts_folder}")
                })?;

            // Prefer locally-stored threading metadata; fall back to the
            // headers carried on the IMAP draft itself.
            let in_reply_to = meta_in_reply_to.clone().or(msg.in_reply_to.clone());
            let references = if !meta_references.is_empty() {
                meta_references.clone()
            } else {
                msg.references
                    .as_deref()
                    .map(envelope_email_transport::threading::parse_references)
                    .unwrap_or_default()
            };

            (
                msg.to_addr,
                msg.subject,
                msg.text_body,
                msg.html_body,
                msg.cc_addr,
                None::<String>,
                None::<String>,
                in_reply_to,
                references,
            )
        }
    } else if let Some(ref d) = local_draft {
        (
            d.to_addr.clone(),
            d.subject.clone().unwrap_or_default(),
            d.text_content.clone(),
            d.html_content.clone(),
            d.cc_addr.clone(),
            d.bcc_addr.clone(),
            d.reply_to.clone(),
            d.in_reply_to.clone().or(meta_in_reply_to.clone()),
            meta_references.clone(),
        )
    } else {
        bail!("draft not found: {id}");
    };

    // Attachments are snapshotted on the local draft at create time, so a draft
    // created with `--attach` re-includes them on send even when content is
    // otherwise fetched from the IMAP copy (which we do not re-parse for bytes).
    let attachments = match local_draft.as_ref() {
        Some(d) => {
            decode_attachments(&d.attachments).context("failed to decode draft attachments")?
        }
        None => Vec::new(),
    };

    // ── Governor gate (fail-closed before any real SMTP) ──
    //
    // This primitive is shared by the CLI `draft send` and MCP `send_draft`
    // surfaces, so both converge on identical blind-attribution semantics. The
    // draft is a persisted, contextual send: threading and attachments are
    // re-derived from what will actually be transmitted.
    let gov_attribution = {
        let gov_req = super::governor_gate::governor_request(
            &acct.id,
            super::governor_gate::account_domain(&creds.account.username),
            &subject,
            &to_addr,
            cc_addr.as_deref(),
            bcc_addr.as_deref(),
            surface,
            Some(id),
            &attachments,
            in_reply_to.is_some(),
            text_body.as_deref(),
            html_body.as_deref(),
            declared,
        );
        // gate_with_attribution refuses an unattributed/invalid request BEFORE
        // Governor is spawned; the send-claim guard releases the draft on bail.
        // The canonical `{status, error}` payload is carried as the error string
        // so the MCP/CLI surface reports structured attribution/Governor recovery.
        let gov_outcome = super::governor_gate::gate_and_record(&db, &acct.id, &gov_req);
        if !gov_outcome.allowed {
            bail!("{}", gov_outcome.response_json());
        }
        gov_outcome.success_attribution()
    };

    // ── Send via SMTP (full path so In-Reply-To / References survive) ──
    // A reply must carry its parent in References even when neither the draft
    // metadata nor the parent's own headers supplied a chain.
    let references = envelope_email_transport::reply::ensure_references_chain(
        &references,
        in_reply_to.as_deref(),
    );
    let references_opt = if references.is_empty() {
        None
    } else {
        Some(references.as_slice())
    };
    let message_id = SmtpSender::send(
        &creds,
        &to_addr,
        &subject,
        text_body.as_deref(),
        html_body.as_deref(),
        None,
        cc_addr.as_deref(),
        bcc_addr.as_deref(),
        reply_to.as_deref(),
        in_reply_to.as_deref(),
        references_opt,
        &attachments,
    )
    .await
    .context("failed to send draft")?;

    let provider_type = db.get_provider_type(&acct.id).ok().flatten();

    // ── Durable sent state FIRST (same post-SMTP discipline as the sweep) ──
    // SMTP was accepted: disarm the claim guard (the claim must never return
    // to `draft`) and persist the sent state via `mark_draft_sent`, which
    // transitions only our held `sending` claim. If persistence fails, park
    // the claim as `blocked` (or leave it inert as `sending`) and SKIP
    // provider cleanup — never report durable success, never leave a
    // retransmit path.
    let lease = claim_guard.as_mut().map(|guard| {
        guard.disarm();
        guard.lease.clone()
    });
    let mut sent_recorded = local_draft.is_none();
    if let (Some(d), Some(lease)) = (&local_draft, &lease) {
        match db.mark_draft_sent(&d.id, lease, Some(&message_id)) {
            Ok(()) => sent_recorded = true,
            Err(e) => {
                let parked = db.park_delivery_uncertain(&d.id, lease).unwrap_or(false);
                warn!(
                    "draft {} was transmitted (message_id={message_id}) but sent-state \
                     persistence failed: {e} — parked as delivery_uncertain={parked}; \
                     provider cleanup skipped. Reconcile explicitly: verify delivery \
                     (Sent folder / recipient), then `envelope draft discard {}`. It \
                     will never be re-sent automatically.",
                    d.id, d.id
                );
            }
        }
    }

    // ── Provider draft cleanup (exact + unique, only after durable state) ──
    // Local drafts resolve identity from the detected-folder cache + the
    // persisted pre-send Message-ID; raw IMAP sends use the folder the content
    // was actually fetched from + the fetched Message-ID. In both cases only
    // the single exact Message-ID match is deleted — never a raw UID in a
    // guessed folder. Skips and failures are logged, never claimed as done.
    let cleanup_target = if !sent_recorded {
        None
    } else if let Some(d) = &local_draft {
        // Identity needs only the exact detected folder + persisted
        // Message-ID; a stored UID is neither required nor trusted.
        match envelope_email_transport::draft_cleanup::resolve_draft_cleanup_target(&db, d) {
            Ok(target) => Some(target),
            Err(reason) => {
                if d.imap_uid.is_some() {
                    warn!("draft {}: provider draft cleanup skipped: {reason}", d.id);
                }
                None
            }
        }
    } else {
        None
    };
    // Reported from the ACTUAL cleanup outcome — never inferred from UID
    // presence or from the absence of local state.
    let mut imap_draft_deleted = false;
    if let Some(target) = cleanup_target
        && !acct.imap_host.is_empty()
    {
        use envelope_email_transport::draft_cleanup::{
            ProviderDraftCleanup, delete_provider_draft_exact,
        };
        match imap::connect(&creds).await {
            Ok(mut client) => match delete_provider_draft_exact(&mut client, &target).await {
                Ok(ProviderDraftCleanup::Deleted { uid }) => {
                    imap_draft_deleted = true;
                    info!(
                        "removed provider draft copy (UID {uid} in {})",
                        target.folder
                    );
                }
                Ok(ProviderDraftCleanup::Skipped(reason)) => {
                    warn!(
                        "provider draft cleanup skipped in {}: {reason}",
                        target.folder
                    );
                }
                Err(e) => warn!("provider draft cleanup failed in {}: {e}", target.folder),
            },
            Err(e) => {
                warn!("failed to connect to IMAP to clean up sent draft: {e}");
            }
        }
    }
    if local_draft.is_some() && !sent_recorded {
        bail!(
            "draft {id} was transmitted (message_id={message_id}) but the sent state could \
             not be recorded; the draft is parked as delivery_uncertain and will never be \
             re-sent. Reconcile explicitly: verify delivery (Sent folder / recipient), \
             then `envelope draft discard {id}`."
        );
    }
    // Catalog event: the send completed. Payload carries only the draft id and
    // message-id transition — never recipients or body.
    let _ = db.emit_catalog_event(
        &creds.account.id,
        envelope_email_store::event_catalog::SEND_COMPLETED,
        Some(serde_json::json!({
            "draft_id": id,
            "message_id": message_id,
        })),
        None,
    );

    // ── Resolve Sent-folder copy (pre-lookup before any client append) ──
    let from = account_from_header(&creds);
    let copy_result = resolve_sent_copy_after_send(
        &db,
        &creds,
        provider_type.as_deref(),
        &from,
        &to_addr,
        &subject,
        text_body.as_deref(),
        html_body.as_deref(),
        cc_addr.as_deref(),
        bcc_addr.as_deref(),
        reply_to.as_deref(),
        in_reply_to.as_deref(),
        &references,
        &message_id,
        &attachments,
    )
    .await;

    let sent_mail_appended = copy_result.sent_mail_appended;
    let sent_mail_append_skipped_reason = copy_result.sent_mail_append_skipped_reason;
    let sent_mail_proof = copy_result.proof;

    // ── Durable Sent-proof annotation (direct/scheduled parity) ──
    // Persist the same dedicated, folder-qualified Sent proof the scheduled sweep
    // records, so an immediate `draft send` / MCP `send_draft` no longer diverges
    // from a scheduled send. Only a durable draft row has anything to persist;
    // a plain direct send with no local draft does not. Strictly AFTER terminal
    // sent persistence and best-effort — a proof failure never retransmits.
    if let Some(d) = &local_draft
        && sent_recorded
    {
        match db.record_sent_copy_proof(
            &d.id,
            sent_mail_proof.folder.as_deref(),
            sent_mail_proof.uid,
            sent_mail_proof.lookup_status,
            sent_mail_proof.copy_source,
        ) {
            Ok(true) => {}
            Ok(false) => warn!(
                "draft {}: Sent-copy proof not recorded (row is not `sent`)",
                d.id
            ),
            Err(e) => warn!("draft {}: failed to record Sent-copy proof: {e}", d.id),
        }
    }

    let (provider_sent_copy, client_appended_copy) =
        sent_copy_convenience_objects(&acct.id, &sent_mail_proof);

    let sent_message_url = sent_mail_proof.message_url(&acct.id);
    let sent_ui = sent_mail_proof.ui(&acct.id);

    let json = serde_json::json!({
        "status": "sent",
        "draft_id": id,
        "to": to_addr.clone(),
        "subject": subject.clone(),
        "message_id": message_id.clone(),
        "imap_draft_deleted": imap_draft_deleted,
        "sent_mail_appended": sent_mail_appended,
        "sent_mail_append_skipped_reason": sent_mail_append_skipped_reason,
        "sent_folder": sent_mail_proof.folder.clone(),
        "sent_uid": sent_mail_proof.uid,
        "sent_message_url": sent_message_url.clone(),
        "sent_mail": sent_mail_proof_json(&acct.id, &sent_mail_proof),
        "provider_sent_copy": provider_sent_copy,
        "client_appended_copy": client_appended_copy,
        "attribution": gov_attribution,
        "ui": sent_ui,
        "draft_ui": ui::draft_ui(&acct.id, id),
    });

    Ok(SentDraftOutcome {
        json,
        to_addr,
        subject,
        message_id,
        sent_folder: sent_mail_proof.folder.clone(),
        sent_uid: sent_mail_proof.uid,
        sent_url: sent_message_url,
        lookup_status: sent_mail_proof.lookup_status,
    })
}

// ─── draft discard ───────────────────────────────────────────────────────

#[tokio::main]
pub async fn run_discard(
    id: &str,
    json: bool,
    account: Option<&str>,
    backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    let is_imap_uid = id.parse::<u32>().is_ok();
    let local_draft = db.get_draft(id).context("failed to get draft")?;

    let imap_uid: Option<u32> = if let Some(ref d) = local_draft {
        d.imap_uid
    } else if is_imap_uid {
        Some(id.parse::<u32>().unwrap())
    } else {
        None
    };

    // ── Delete from IMAP Drafts folder (primary) ──
    if let Some(uid) = imap_uid {
        let passphrase = credential_store::get_or_create_passphrase(backend)
            .context("credential store error")?;

        let acct = match account {
            Some(a) => resolve_account(&db, Some(a))?,
            None => {
                if let Some(ref d) = local_draft {
                    db.get_account(&d.account_id)
                        .context("database error")?
                        .ok_or_else(|| {
                            anyhow::anyhow!("account not found for draft: {}", d.account_id)
                        })?
                } else {
                    let acct = db
                        .default_account()
                        .context("failed to query default account")?;
                    acct.ok_or_else(|| {
                        anyhow::anyhow!("no --account specified and no default account")
                    })?
                }
            }
        };

        if !acct.imap_host.is_empty() {
            let creds = db
                .get_account_with_credentials(&acct.id, &passphrase)
                .context("failed to decrypt credentials")?;

            match imap::connect(&creds).await {
                Ok(mut client) => {
                    let drafts_folder = detect_drafts_folder(&mut client, &db, &acct.id)
                        .await
                        .map_err(|e| anyhow::anyhow!("drafts folder detection failed: {e}"))?
                        .unwrap_or_else(|| "Drafts".to_string());

                    if let Err(e) = imap::delete_message(&mut client, &drafts_folder, uid).await {
                        warn!(
                            "failed to delete draft from IMAP {} (UID {uid}): {e}",
                            drafts_folder
                        );
                    }
                }
                Err(e) => {
                    warn!("failed to connect to IMAP to discard draft: {e}");
                }
            }
        }
    }

    // ── Delete local SQLite record (secondary) ──
    if local_draft.is_some() {
        let discarded = db.discard_draft(id).context("failed to discard draft")?;
        if !discarded {
            warn!("local draft {id} was not discardable (status may have changed)");
        }
    } else if !is_imap_uid {
        bail!("draft not found: {id}");
    }

    if json {
        let ui_meta = local_draft
            .as_ref()
            .map(|d| ui::draft_ui(&d.account_id, id))
            .unwrap_or_else(ui::root_ui);
        println!(
            "{}",
            serde_json::json!({
                "action": "discard",
                "draft_id": id,
                "imap_deleted": imap_uid.is_some(),
                "local_deleted": local_draft.is_some(),
                "ui": ui_meta,
            })
        );
    } else {
        println!("Draft {id} discarded");
        if imap_uid.is_some() {
            println!("  IMAP: deleted from Drafts folder");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::models::DraftStatus;

    #[test]
    fn draft_dashboard_path_encodes_account_and_draft_segments() {
        assert_eq!(
            draft_dashboard_path("editor@spainexpat.com", "draft id/1"),
            "/accounts/editor%40spainexpat.com/drafts/draft%20id%2F1"
        );
    }

    #[test]
    fn draft_dashboard_url_uses_supplied_base_without_double_slash() {
        assert_eq!(
            draft_dashboard_url_with_base(
                "http://localhost:1111/",
                "editor@spainexpat.com",
                "draft-123",
            ),
            "http://localhost:1111/accounts/editor%40spainexpat.com/drafts/draft-123"
        );
    }

    fn drafts_test_config_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "envelope-drafts-dashboard-test-{}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn draft_urls_use_persisted_dashboard_base_url() {
        let path = drafts_test_config_path("persisted-base");
        let _ = std::fs::remove_file(&path);
        let _guard = crate::commands::config::isolated_dashboard_config(path.clone());
        std::fs::write(
            &path,
            r#"{"dashboard":{"base_url":"https://macbook-pro.tail87a011.ts.net"}}"#,
        )
        .unwrap();

        let account = "editor@spainexpat.com";
        let draft = "draft-123";
        let top_level = draft_dashboard_url(account, draft);
        let ui = ui::draft_ui(account, draft);

        let expected = "https://macbook-pro.tail87a011.ts.net/accounts/editor%40spainexpat.com/drafts/draft-123";
        assert_eq!(top_level, expected);
        assert_eq!(ui["review_url"], expected);
        assert_eq!(ui["dashboard_url"], "https://macbook-pro.tail87a011.ts.net");
        // Top-level draft URL must agree with the nested UI review URL.
        assert_eq!(top_level, ui["review_url"].as_str().unwrap());
    }

    #[test]
    fn draft_urls_prefer_env_over_persisted_dashboard_base_url() {
        let path = drafts_test_config_path("env-precedence");
        let _ = std::fs::remove_file(&path);
        let guard = crate::commands::config::isolated_dashboard_config(path.clone());
        std::fs::write(
            &path,
            r#"{"dashboard":{"base_url":"https://config.example"}}"#,
        )
        .unwrap();
        guard.set_primary_env("https://env.example/");

        let account = "a@b.com";
        let draft = "d1";
        let top_level = draft_dashboard_url(account, draft);
        let ui = ui::draft_ui(account, draft);

        assert_eq!(
            top_level,
            "https://env.example/accounts/a%40b.com/drafts/d1"
        );
        assert_eq!(top_level, ui["review_url"].as_str().unwrap());
    }

    #[test]
    fn gmail_smtp_auto_saves_sent_mail() {
        assert!(provider_auto_saves_sent(Some("gmail"), "smtp.gmail.com"));
        assert!(provider_auto_saves_sent(None, "smtp.gmail.com"));
        assert!(provider_auto_saves_sent(
            Some("google_workspace"),
            "smtp.example.com"
        ));
    }

    #[test]
    fn generic_smtp_still_needs_sent_append() {
        assert!(!provider_auto_saves_sent(Some("migadu"), "smtp.migadu.com"));
        assert!(!provider_auto_saves_sent(None, "mail.example.com"));
    }

    #[test]
    fn sent_mail_proof_json_exposes_uid_and_message_url_when_found() {
        let proof = SentMailProof::new(Some("Sent Messages".to_string()), Some(42), "found", None);
        let value = sent_mail_proof_json("acct@example.com", &proof);

        assert_eq!(value["folder"], "Sent Messages");
        assert_eq!(value["uid"], 42);
        assert_eq!(value["lookup_status"], "found");
        assert!(
            value["message_url"]
                .as_str()
                .unwrap()
                .ends_with("/mail/unified/acct%40example.com/42?folder=Sent%20Messages")
        );
        assert!(
            value["ui"]["message_url"]
                .as_str()
                .unwrap()
                .contains("folder=Sent%20Messages")
        );
    }

    #[test]
    fn sent_mail_proof_json_reports_null_uid_with_lookup_reason() {
        let proof = SentMailProof::new(
            Some("Sent".to_string()),
            None,
            "not_found",
            Some("not indexed yet".to_string()),
        );
        let value = sent_mail_proof_json("acct@example.com", &proof);

        assert_eq!(value["folder"], "Sent");
        assert!(value["uid"].is_null());
        assert!(value["message_url"].is_null());
        assert_eq!(value["lookup_status"], "not_found");
        assert_eq!(value["lookup_error"], "not indexed yet");
        assert!(value["ui"]["cockpit_url"].as_str().is_some());
    }

    #[test]
    fn draft_rfc822_accepts_multiple_recipients_and_cc() {
        let (rfc822, _) = build_rfc822_draft(
            "Agent <agent@example.com>",
            "Alice <a@example.com>, b@example.com",
            Some("Multiple recipients"),
            Some("hello"),
            Some("c@example.com, \"Dee Ops\" <d@example.com>"),
            Some("hidden@example.com"),
            None,
            &[],
        )
        .unwrap();
        let msg = String::from_utf8(rfc822).unwrap();

        assert!(msg.contains("a@example.com"));
        assert!(msg.contains("b@example.com"));
        assert!(msg.contains("c@example.com"));
        assert!(msg.contains("d@example.com"));
        assert!(msg.contains("hidden@example.com"));
        assert!(!msg.contains("<a@example.com, b@example.com>"));
    }

    #[test]
    fn draft_rfc822_includes_attachments() {
        let attachment = Attachment {
            filename: "hello.txt".to_string(),
            content_type: "text/plain".to_string(),
            data: b"hello attachment".to_vec(),
        };
        let (rfc822, _) = build_rfc822_draft(
            "agent@example.com",
            "a@example.com",
            Some("Attached"),
            Some("see attached"),
            None,
            None,
            None,
            &[attachment],
        )
        .unwrap();
        let msg = String::from_utf8(rfc822).unwrap();

        assert!(msg.contains("multipart/mixed"));
        assert!(msg.contains("hello.txt"));
        assert!(msg.contains("hello attachment") || msg.contains("aGVsbG8gYXR0YWNobWVudA"));
    }

    // ─── appended Sent copy From header (issue #81) ──────────────────────

    /// Extract the raw `From:` header line from a built RFC822 message.
    fn from_header_line(msg: &str) -> String {
        msg.lines()
            .find(|l| l.to_lowercase().starts_with("from:"))
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn appended_sent_from_uses_account_name_fallback_not_nested() {
        // account_from_header produces the preformatted mailbox that the Sent
        // copy is built from. It must not be double-wrapped into nested angle
        // brackets when re-serialized by build_rfc822_full.
        let creds = make_creds("user@example.test", None, "Display Name");
        let from = account_from_header(&creds);

        let (rfc822, _) = build_rfc822_full(
            &from,
            "recipient@example.test",
            "Subject",
            Some("body"),
            None,
            None,
            None,
            &[],
            Some("<mid@example.test>"),
            &[],
        )
        .unwrap();
        let msg = String::from_utf8(rfc822).unwrap();
        let from_line = from_header_line(&msg);

        assert_eq!(
            from_line, "From: \"Display Name\" <user@example.test>",
            "account-name fallback must serialize as a proper mailbox, not nested: {from_line}"
        );
        assert!(
            !from_line.contains("<Display Name <"),
            "From must not be double-wrapped: {from_line}"
        );
    }

    #[test]
    fn appended_sent_from_quotes_comma_display_name() {
        let creds = make_creds("user@example.test", Some("Doe, Jane \"JD\""), "Account");
        let from = account_from_header(&creds);

        let (rfc822, _) = build_rfc822_full(
            &from,
            "recipient@example.test",
            "Subject",
            Some("body"),
            None,
            None,
            None,
            &[],
            Some("<mid@example.test>"),
            &[],
        )
        .unwrap();
        let msg = String::from_utf8(rfc822).unwrap();
        let from_line = from_header_line(&msg);

        assert!(
            from_line.contains("user@example.test"),
            "address must be present: {from_line}"
        );
        assert!(
            !from_line.contains("<Doe,"),
            "comma/quote display name must not leak into the address wrapper: {from_line}"
        );
        // Round-trips back into a single valid mailbox.
        let parsed = from_line
            .trim_start_matches("From:")
            .trim()
            .parse::<Mailboxes>()
            .expect("From header must be a valid RFC5322 mailbox");
        assert_eq!(parsed.iter().count(), 1);
    }

    #[test]
    fn appended_sent_from_explicit_override_not_double_wrapped() {
        let (rfc822, _) = build_rfc822_full(
            "\"Override Name\" <override@example.test>",
            "recipient@example.test",
            "Subject",
            Some("body"),
            None,
            None,
            None,
            &[],
            Some("<mid@example.test>"),
            &[],
        )
        .unwrap();
        let msg = String::from_utf8(rfc822).unwrap();
        let from_line = from_header_line(&msg);

        assert_eq!(
            from_line, "From: \"Override Name\" <override@example.test>",
            "explicit --from override must not be double-wrapped: {from_line}"
        );
    }

    #[test]
    fn appended_sent_from_preserves_attachments() {
        let attachment = Attachment {
            filename: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            data: b"%PDF-1.4 fake".to_vec(),
        };
        let creds = make_creds("user@example.test", None, "Display Name");
        let from = account_from_header(&creds);

        let (rfc822, _) = build_rfc822_full(
            &from,
            "recipient@example.test",
            "Subject",
            Some("body"),
            None,
            None,
            None,
            &[],
            Some("<mid@example.test>"),
            &[attachment],
        )
        .unwrap();
        let msg = String::from_utf8(rfc822).unwrap();
        let from_line = from_header_line(&msg);

        assert_eq!(from_line, "From: \"Display Name\" <user@example.test>");
        assert!(msg.contains("multipart/mixed"));
        assert!(msg.contains("report.pdf"));
    }

    // ─── account_from_header / From identity ─────────────────────────────

    fn make_creds(
        username: &str,
        display_name: Option<&str>,
        name: &str,
    ) -> AccountWithCredentials {
        use envelope_email_store::models::Account;
        AccountWithCredentials {
            account: Account {
                id: "acct-test".to_string(),
                name: name.to_string(),
                username: username.to_string(),
                domain: String::new(),
                smtp_host: "smtp.example.com".to_string(),
                smtp_port: 587,
                imap_host: "imap.example.com".to_string(),
                imap_port: 993,
                smtp_username: None,
                imap_username: None,
                display_name: display_name.map(str::to_string),
                signature_text: None,
                signature_html: None,
                created_at: String::new(),
            },
            password: "unused".to_string(),
            smtp_password: None,
            imap_password: None,
        }
    }

    #[test]
    fn from_header_display_name_wins_over_account_name() {
        let creds = make_creds("tyler@martin.fm", Some("Display Name"), "Account Name");
        let from = account_from_header(&creds);
        assert!(
            from.contains("Display Name"),
            "display_name should win: {from}"
        );
        assert!(
            !from.contains("Account Name"),
            "account name must not appear: {from}"
        );
        assert!(
            from.contains("tyler@martin.fm"),
            "address must be present: {from}"
        );
    }

    #[test]
    fn append_draft_required_fails_loud_for_send_only_account() {
        // A send-only account (no IMAP host) has nowhere for a draft to land
        // where a mail client can see it. Rather than silently create a
        // local-only phantom, the append must fail loud.
        let db = envelope_email_store::Database::open_memory().unwrap();
        let mut creds = make_creds("send-only@example.com", None, "Send Only");
        creds.account.imap_host = String::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(append_draft_required(
                &db,
                &creds,
                b"raw message",
                "<mid@example.com>",
            ))
            .expect_err("send-only account must not yield a silent local-only draft");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no IMAP mailbox"),
            "expected fail-loud IMAP message, got: {msg}"
        );
    }

    #[test]
    fn from_header_falls_back_to_account_name_when_no_display_name() {
        let creds = make_creds("tyler@martin.fm", None, "Tyler Martin");
        let from = account_from_header(&creds);
        assert!(
            from.contains("Tyler Martin"),
            "account name fallback required: {from}"
        );
        assert!(
            from.contains("tyler@martin.fm"),
            "address must be present: {from}"
        );
    }

    #[test]
    fn from_header_blank_display_name_uses_account_name_fallback() {
        let creds = make_creds("tyler@martin.fm", Some("  "), "Tyler Martin");
        let from = account_from_header(&creds);
        assert!(
            from.contains("Tyler Martin"),
            "blank display_name must not suppress name: {from}"
        );
    }

    #[test]
    fn from_header_omits_name_when_account_name_equals_email() {
        let creds = make_creds("tyler@martin.fm", None, "tyler@martin.fm");
        let from = account_from_header(&creds);
        assert!(
            !from.contains("tyler@martin.fm <tyler@martin.fm>"),
            "redundant name must not appear: {from}"
        );
    }

    #[test]
    fn from_header_quotes_account_name_with_comma() {
        let creds = make_creds("tyler@martin.fm", None, "Martin, Tyler");
        let from = account_from_header(&creds);
        assert!(
            from.contains("\"Martin, Tyler\""),
            "comma in name must be quoted: {from}"
        );
    }

    // ─── draft_envelope_json: sync_status_reason ─────────────────────────

    /// APPEND-succeeded/UID-missing provenance: a successful APPEND whose
    /// post-APPEND UID lookup returned None leaves `imap_uid` NULL while
    /// `storage.imap_synced=true` records that a provider copy exists. The
    /// modify replace decision must treat that as "copy may exist" (old-copy
    /// cleanup required before APPEND); only genuinely local-only drafts may
    /// APPEND without it. Pure store rows — no mailbox or network.
    #[test]
    fn provider_copy_presence_uses_storage_provenance_not_uid_alone() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'T', 't@example.com', 'example.com',
                         'smtp.example.com', 587, 'imap.example.com', 993, 'enc')",
                [],
            )
            .unwrap();
        let draft = db
            .create_draft(
                "acc1",
                "to@example.net",
                Some("S"),
                Some("b"),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();

        // Fresh draft: no UID, no storage provenance → genuinely local-only.
        let fresh = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(!provider_copy_may_exist(&fresh));

        // APPEND succeeded but the UID lookup missed: imap_uid stays NULL,
        // storage provenance records the sync. A copy MAY exist.
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({
                "storage": {"imap_synced": true, "imap_folder": "[Gmail]/Drafts", "local_only": false}
            }),
        )
        .unwrap();
        let appended_no_uid = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(
            appended_no_uid.imap_uid.is_none(),
            "precondition: UID lookup missed"
        );
        assert!(
            provider_copy_may_exist(&appended_no_uid),
            "imap_synced provenance must imply a possible provider copy despite a NULL UID"
        );

        // Explicit local-only provenance: append is allowed without cleanup.
        db.set_draft_metadata(
            &draft.id,
            &serde_json::json!({"storage": {"imap_synced": false, "local_only": true}}),
        )
        .unwrap();
        let local_only = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(!provider_copy_may_exist(&local_only));

        // A recorded UID always implies a possible copy, regardless of metadata.
        db.update_draft_imap_uid(&draft.id, 77).unwrap();
        let with_uid = db.get_draft(&draft.id).unwrap().unwrap();
        assert!(provider_copy_may_exist(&with_uid));
    }

    fn seed_account_and_bot_draft(
        db: &envelope_email_store::Database,
        to: &str,
    ) -> envelope_email_store::models::Draft {
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES ('acc1', 'T', 't@example.com', 'example.com',
                         'smtp.example.com', 587, 'imap.example.com', 993, 'enc')",
                [],
            )
            .unwrap();
        db.create_draft(
            "acc1",
            to,
            Some("S"),
            Some("b"),
            None,
            None,
            None,
            None,
            Some("mcp"),
        )
        .unwrap()
    }

    /// The validated revision reaches the CAS on the happy path: precheck resolves
    /// the declaration against revision R, and queueing at R (no concurrent edit)
    /// schedules the draft with the declaration bound to R.
    #[test]
    fn precheck_revision_reaches_queue_cas_and_binds_declaration() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let draft = seed_account_and_bot_draft(&db, "safe@partner.example");
        let declared = vec!["informational".to_string()];

        let precheck = precheck_draft(&db, &draft.id, SendSurface::Mcp, &declared, None)
            .expect("precheck loads draft + account");
        assert!(
            precheck.refusal.is_none(),
            "a valid declaration passes the precheck"
        );
        assert!(precheck.resolution.is_attributed());

        queue_bot_draft_for_send(
            &db,
            &draft.id,
            precheck.revision,
            "2000-01-01T00:00:00Z",
            &declared,
        )
        .expect("queue at the validated revision succeeds");

        let reloaded = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(reloaded.status, DraftStatus::Draft, "scheduled + due");
        assert_eq!(reloaded.send_after.as_deref(), Some("2000-01-01T00:00:00Z"));
        let attr = reloaded.metadata.unwrap()["attribution"].clone();
        assert_eq!(attr["declared_attrs"][0], "informational");
        assert_eq!(
            attr["revision"], precheck.revision,
            "declaration bound to the validated revision"
        );
    }

    /// Handler-level race (the exact Codex path): a concurrent recipient edit
    /// after precheck bumps the revision. Because the queue CAS binds to the
    /// EXACT validated revision (never a reload), it must conflict and leave the
    /// draft un-scheduled, un-attributed, and NOT resurrected — the stale
    /// declaration is never bound to the edited content.
    #[test]
    fn precheck_then_concurrent_edit_conflicts_without_binding_stale_declaration() {
        let db = envelope_email_store::Database::open_memory().unwrap();
        let draft = seed_account_and_bot_draft(&db, "safe@partner.example");
        let declared = vec!["informational".to_string()];

        // 1) Bot validates its declaration against the current revision.
        let precheck = precheck_draft(&db, &draft.id, SendSurface::Mcp, &declared, None)
            .expect("precheck loads");
        assert!(precheck.refusal.is_none());
        let validated_rev = precheck.revision;

        // 2) A concurrent material edit lands BEFORE the queue CAS.
        db.update_draft_content(
            &draft.id,
            Some("attacker@evil.example"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let edited = db.get_draft(&draft.id).unwrap().unwrap();
        assert_ne!(
            edited.revision, validated_rev,
            "the edit bumped the revision"
        );

        // 3) Queue at the VALIDATED revision → conflict; nothing is bound.
        let err = queue_bot_draft_for_send(
            &db,
            &draft.id,
            validated_rev,
            "2000-01-01T00:00:00Z",
            &declared,
        )
        .expect_err("a stale-revision queue must conflict");
        assert!(
            matches!(
                err.downcast_ref::<envelope_email_store::StoreError>(),
                Some(envelope_email_store::StoreError::DraftModifiedConcurrently(
                    _
                ))
            ),
            "expected a concurrent-modification conflict, got: {err:?}"
        );

        // The edited draft was NOT scheduled and carries NO stale declaration.
        let after = db.get_draft(&draft.id).unwrap().unwrap();
        assert_eq!(after.to_addr, "attacker@evil.example", "the edit persisted");
        assert_eq!(
            after.status,
            DraftStatus::Draft,
            "not resurrected/transitioned by the failed queue"
        );
        assert!(
            after.send_after.is_none(),
            "no schedule was bound to the edited content"
        );
        assert!(
            after
                .metadata
                .and_then(|m| m.get("attribution").cloned())
                .is_none(),
            "the stale declaration was never bound to the edited content"
        );
    }

    /// A mismatched --account/credential context must refuse before any
    /// provider or network side effect. Pure — no DB, no IMAP, no SMTP.
    #[test]
    fn account_binding_refuses_mismatched_credentials() {
        assert!(ensure_draft_account_binding("d1", "acc1", "acc1", "a@x.com").is_ok());
        let err = ensure_draft_account_binding("d1", "acc1", "acc2", "b@y.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("belongs to account acc1"), "{err}");
        assert!(err.contains("b@y.com"), "{err}");
        assert!(
            err.contains("refusing before"),
            "refusal must be explicit about ordering: {err}"
        );
    }

    #[test]
    fn draft_envelope_json_exposes_sync_status_reason_when_local_only() {
        let draft = envelope_email_store::models::Draft {
            id: "d1".to_string(),
            account_id: "acct@example.com".to_string(),
            status: DraftStatus::Draft,
            to_addr: "b@example.com".to_string(),
            cc_addr: None,
            bcc_addr: None,
            reply_to: None,
            subject: Some("Test".to_string()),
            text_content: Some("hi".to_string()),
            html_content: None,
            in_reply_to: None,
            metadata: Some(serde_json::json!({
                "storage": {
                    "imap_synced": false,
                    "local_only": true,
                    "sync_status_reason": "imap_sync_failed",
                }
            })),
            attachments: vec![],
            message_id: None,
            send_after: None,
            snoozed_until: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            sent_at: None,
            created_by: Some("cli".to_string()),
            imap_uid: None,
            revision: 0,
        };
        let value = draft_envelope_json(&draft);
        assert_eq!(
            value["storage"]["local_only"], true,
            "local_only must be true"
        );
        assert_eq!(
            value["storage"]["sync_status_reason"], "imap_sync_failed",
            "sync_status_reason must be surfaced in storage block"
        );
        // An ordinary local draft (no send_after) reports `drafted`.
        assert_eq!(value["status"], "drafted");
    }

    /// Real evidence: `draft show` reported a durably SENT / PENDING-REVIEW /
    /// QUEUED draft as an ordinary `drafted` local draft. The envelope status must
    /// reflect the true persisted `DraftStatus` (and `send_after` for a queue).
    #[test]
    fn draft_envelope_status_reflects_true_persisted_status() {
        fn draft_with(status: DraftStatus, send_after: Option<&str>) -> Draft {
            Draft {
                id: "d1".to_string(),
                account_id: "acct@example.com".to_string(),
                status,
                to_addr: "b@example.com".to_string(),
                cc_addr: None,
                bcc_addr: None,
                reply_to: None,
                subject: Some("Test".to_string()),
                text_content: Some("hi".to_string()),
                html_content: None,
                in_reply_to: None,
                metadata: None,
                attachments: vec![],
                message_id: None,
                send_after: send_after.map(str::to_string),
                snoozed_until: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                sent_at: None,
                created_by: Some("cli".to_string()),
                imap_uid: None,
                revision: 0,
            }
        }

        assert_eq!(
            draft_envelope_json(&draft_with(DraftStatus::Sent, None))["status"],
            "sent"
        );
        assert_eq!(
            draft_envelope_json(&draft_with(DraftStatus::PendingReview, None))["status"],
            "pending_review"
        );
        // A `draft` row carrying a schedule is queued, not an ordinary draft.
        assert_eq!(
            draft_envelope_json(&draft_with(
                DraftStatus::Draft,
                Some("2026-01-01T00:02:00Z")
            ))["status"],
            "queued"
        );
        // An ordinary local draft with no schedule stays `drafted`.
        assert_eq!(
            draft_envelope_json(&draft_with(DraftStatus::Draft, None))["status"],
            "drafted"
        );
        assert_eq!(
            draft_envelope_json(&draft_with(DraftStatus::Blocked, None))["status"],
            "blocked"
        );
    }

    /// A reply whose parent lives only in the `in_reply_to` column (no
    /// contextual-reply metadata blob) is still a reply. Reading threading
    /// from metadata alone reported it as `draft_kind: "new"` with
    /// `in_reply_to: null`, so the agent contract disowned the thread and the
    /// send path had nothing to re-emit.
    #[test]
    fn draft_threading_falls_back_to_the_in_reply_to_column() {
        let mut draft = Draft {
            id: "draft-1".to_string(),
            account_id: "acct@example.com".to_string(),
            status: DraftStatus::Draft,
            to_addr: "a@example.com".to_string(),
            cc_addr: None,
            bcc_addr: None,
            reply_to: None,
            subject: Some("RE: thread".to_string()),
            text_content: Some("body".to_string()),
            html_content: None,
            in_reply_to: Some("<parent@example.net>".to_string()),
            metadata: None,
            attachments: vec![],
            message_id: Some("<mine@example.net>".to_string()),
            send_after: None,
            snoozed_until: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            sent_at: None,
            created_by: Some("cli".to_string()),
            imap_uid: Some(2084),
            revision: 0,
        };

        let (irt, refs, mid) = threading_for_draft(&draft);
        assert_eq!(irt.as_deref(), Some("<parent@example.net>"));
        assert_eq!(mid.as_deref(), Some("<mine@example.net>"));
        assert!(refs.is_empty());

        let value = draft_envelope_json(&draft);
        assert_eq!(value["fields"]["in_reply_to"], "<parent@example.net>");
        assert_eq!(
            value["draft_kind"], "reply",
            "a draft with a parent must not report itself as a new message"
        );

        // Metadata still wins when it carries the richer contextual state.
        draft.metadata = Some(serde_json::json!({
            "draft_kind": "forward",
            "in_reply_to": "<meta-parent@example.net>",
            "references": ["<root@example.net>", "<meta-parent@example.net>"],
        }));
        let (irt, refs, _) = threading_for_draft(&draft);
        assert_eq!(irt.as_deref(), Some("<meta-parent@example.net>"));
        assert_eq!(refs.len(), 2);
        assert_eq!(draft_envelope_json(&draft)["draft_kind"], "forward");

        // A genuine fresh draft keeps reporting itself as new.
        draft.in_reply_to = None;
        draft.metadata = None;
        assert_eq!(draft_envelope_json(&draft)["draft_kind"], "new");
        assert!(threading_for_draft(&draft).0.is_none());
    }

    #[test]
    fn draft_envelope_json_reports_attachment_summaries_without_bytes() {
        let draft = Draft {
            id: "draft-1".to_string(),
            account_id: "acct@example.com".to_string(),
            status: DraftStatus::Draft,
            to_addr: "a@example.com".to_string(),
            cc_addr: None,
            bcc_addr: None,
            reply_to: None,
            subject: Some("With attachment".to_string()),
            text_content: Some("body".to_string()),
            html_content: None,
            in_reply_to: None,
            metadata: Some(serde_json::json!({"draft_kind": "new"})),
            attachments: vec![serde_json::json!({
                "filename": "secret.txt",
                "content_type": "text/plain",
                "size": 5,
                "data_base64": "aGVsbG8=",
            })],
            message_id: None,
            send_after: None,
            snoozed_until: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            sent_at: None,
            created_by: Some("test".to_string()),
            imap_uid: None,
            revision: 0,
        };

        let value = draft_envelope_json(&draft);
        let serialized = serde_json::to_string(&value).unwrap();

        assert_eq!(value["attachments"][0]["filename"], "secret.txt");
        assert_eq!(value["attachments"][0]["size"], 5);
        assert!(!serialized.contains("data_base64"));
        assert!(!serialized.contains("aGVsbG8="));
    }

    // ─── copy_source semantics (issue #77) ───────────────────────────────

    #[test]
    fn copy_source_is_provider_when_auto_saved_and_lookup_found() {
        assert_eq!(
            determine_copy_source(true, true, false, false, true),
            "provider"
        );
    }

    #[test]
    fn copy_source_is_unresolved_when_auto_saved_but_lookup_not_found() {
        assert_eq!(
            determine_copy_source(true, true, false, false, false),
            "unresolved"
        );
    }

    #[test]
    fn copy_source_is_client_appended_when_client_wrote_archive_and_lookup_found() {
        assert_eq!(
            determine_copy_source(true, false, true, true, true),
            "client_appended"
        );
    }

    #[test]
    fn copy_source_is_client_appended_when_append_done_but_lookup_not_found() {
        // Lookup may still be delayed; source is still client_appended.
        assert_eq!(
            determine_copy_source(true, false, true, true, false),
            "client_appended"
        );
    }

    #[test]
    fn copy_source_is_not_attempted_when_no_imap() {
        assert_eq!(
            determine_copy_source(false, false, false, false, false),
            "not_attempted"
        );
    }

    #[test]
    fn copy_source_not_attempted_overrides_provider_auto_saves_when_no_imap() {
        // No IMAP means we couldn't verify provider copy either.
        assert_eq!(
            determine_copy_source(false, true, false, false, false),
            "not_attempted"
        );
    }

    #[test]
    fn sent_mail_proof_json_includes_copy_source_field() {
        let proof = SentMailProof {
            folder: Some("Sent".to_string()),
            uid: Some(10),
            lookup_status: "found",
            lookup_error: None,
            copy_source: "provider",
        };
        let value = sent_mail_proof_json("acct@example.com", &proof);
        assert_eq!(value["copy_source"], "provider");
    }

    #[test]
    fn sent_mail_proof_json_copy_source_client_appended() {
        let proof = SentMailProof {
            folder: Some("Sent".to_string()),
            uid: Some(99),
            lookup_status: "found",
            lookup_error: None,
            copy_source: "client_appended",
        };
        let value = sent_mail_proof_json("acct@example.com", &proof);
        assert_eq!(value["copy_source"], "client_appended");
        // Existing backward-compat fields must still be present.
        assert_eq!(value["uid"], 99);
        assert_eq!(value["lookup_status"], "found");
    }

    #[test]
    fn sent_mail_proof_json_copy_source_not_attempted() {
        let proof = SentMailProof {
            folder: None,
            uid: None,
            lookup_status: "no_imap",
            lookup_error: None,
            copy_source: "not_attempted",
        };
        let value = sent_mail_proof_json("acct@example.com", &proof);
        assert_eq!(value["copy_source"], "not_attempted");
        assert!(value["uid"].is_null());
    }

    #[test]
    fn direct_draft_send_persists_sent_proof_after_resolving_it() {
        // Finding 3 (direct side of direct/scheduled durable parity): the shared
        // durable draft-send path (send_existing_draft — CLI `draft send` and MCP
        // `send_draft`) must persist the resolved Sent proof, not resolve it and
        // drop it as before. Ordering: resolve first, then annotate. A full send
        // needs live SMTP/IMAP, so guard the wiring/ordering at the source
        // boundary (mirrors `cli_send_no_longer_calls_append_helper_directly`).
        let src = include_str!("drafts.rs");
        let fn_start = src
            .find("pub(crate) async fn send_existing_draft")
            .expect("shared durable draft-send helper present");
        let body = &src[fn_start..];
        let resolve_at = body
            .find("resolve_sent_copy_after_send(")
            .expect("direct path resolves the Sent copy");
        let record_at = body
            .find("record_sent_copy_proof(")
            .expect("direct path records the Sent proof durably");
        assert!(
            record_at > resolve_at,
            "must resolve the Sent copy before persisting the proof"
        );
    }

    // ─── decide_sent_copy_action (issue #77 pre-lookup semantics) ────────────

    #[test]
    fn decide_sent_copy_no_imap_always_returns_no_imap() {
        assert_eq!(
            decide_sent_copy_action(false, false, "not_found"),
            SentCopyDecision::NoImap
        );
        assert_eq!(
            decide_sent_copy_action(false, true, "found"),
            SentCopyDecision::NoImap
        );
    }

    #[test]
    fn decide_sent_copy_pre_lookup_found_means_provider_copy() {
        // Exact unique copy found before any append → provider placed it.
        assert_eq!(
            decide_sent_copy_action(true, false, "found"),
            SentCopyDecision::ProviderFound
        );
        assert_eq!(
            decide_sent_copy_action(true, true, "found"),
            SentCopyDecision::ProviderFound
        );
    }

    #[test]
    fn decide_sent_copy_auto_saves_but_lookup_missed_is_unresolved() {
        assert_eq!(
            decide_sent_copy_action(true, true, "not_found"),
            SentCopyDecision::ProviderUnresolved
        );
    }

    #[test]
    fn decide_sent_copy_no_auto_save_and_not_found_needs_client_append() {
        assert_eq!(
            decide_sent_copy_action(true, false, "not_found"),
            SentCopyDecision::NeedsClientAppend
        );
    }

    #[test]
    fn decide_sent_copy_ambiguous_never_appends() {
        // Multiple exact copies already exist: never APPEND another archive.
        for auto_saves in [false, true] {
            assert_eq!(
                decide_sent_copy_action(true, auto_saves, "ambiguous"),
                SentCopyDecision::Unresolved("ambiguous_sent_copies")
            );
        }
    }

    #[test]
    fn decide_sent_copy_inconclusive_lookup_never_appends() {
        for status in ["lookup_failed", "imap_connect_failed", "no_message_id"] {
            assert_eq!(
                decide_sent_copy_action(true, false, status),
                SentCopyDecision::Unresolved("sent_lookup_inconclusive")
            );
        }
    }

    // ─── sent_copy_convenience_objects projection (never mislabel unresolved) ──

    fn proof_with_source(source: &'static str) -> SentMailProof {
        SentMailProof {
            folder: Some("Sent".to_string()),
            uid: Some(7),
            lookup_status: "found",
            lookup_error: None,
            copy_source: source,
        }
    }

    #[test]
    fn convenience_provider_populates_only_provider_sent_copy() {
        let (provider, client) =
            sent_copy_convenience_objects("acct@example.com", &proof_with_source("provider"));
        assert!(
            provider.is_some(),
            "provider copy_source → provider_sent_copy"
        );
        assert!(
            client.is_none(),
            "provider copy_source → no client_appended_copy"
        );
        assert_eq!(provider.unwrap()["copy_source"], "provider");
    }

    #[test]
    fn convenience_client_appended_populates_only_client_appended_copy() {
        let (provider, client) = sent_copy_convenience_objects(
            "acct@example.com",
            &proof_with_source("client_appended"),
        );
        assert!(
            provider.is_none(),
            "client_appended → no provider_sent_copy"
        );
        assert!(client.is_some(), "client_appended → client_appended_copy");
        assert_eq!(client.unwrap()["copy_source"], "client_appended");
    }

    #[test]
    fn convenience_unresolved_is_never_presented_as_provider_proof() {
        // Blocker regression: `unresolved` (e.g. a generic-provider APPEND
        // failure) must never be surfaced as provider proof. Both convenience
        // objects are null; the canonical `sent_mail` still carries copy_source.
        let (provider, client) =
            sent_copy_convenience_objects("acct@example.com", &proof_with_source("unresolved"));
        assert!(
            provider.is_none(),
            "unresolved must not be presented as provider_sent_copy"
        );
        assert!(
            client.is_none(),
            "unresolved must not be client_appended_copy"
        );
    }

    #[test]
    fn convenience_generic_provider_append_failure_yields_null_provider_copy() {
        // A generic-provider APPEND failure resolves as `unresolved` with a null
        // UID; the projection must not fabricate provider proof from it.
        let proof = SentMailProof {
            folder: Some("Sent".to_string()),
            uid: None,
            lookup_status: "not_found",
            lookup_error: None,
            copy_source: "unresolved",
        };
        let (provider, client) = sent_copy_convenience_objects("acct@example.com", &proof);
        assert!(provider.is_none());
        assert!(client.is_none());
    }

    #[test]
    fn convenience_not_attempted_leaves_both_objects_null() {
        let proof = SentMailProof {
            folder: None,
            uid: None,
            lookup_status: "no_imap",
            lookup_error: None,
            copy_source: "not_attempted",
        };
        let (provider, client) = sent_copy_convenience_objects("acct@example.com", &proof);
        assert!(provider.is_none());
        assert!(client.is_none());
    }

    // ─── bare newline rejection on IMAP APPEND (issue #87) ──────────────

    /// Fail if the serialized message contains a LF not preceded by CR, or a
    /// CR not followed by LF — servers reject either as "bare newlines".
    fn assert_strict_crlf(rfc822: &[u8]) {
        for (i, &b) in rfc822.iter().enumerate() {
            if b == b'\n' && (i == 0 || rfc822[i - 1] != b'\r') {
                panic!(
                    "bare LF at byte {i}: ...{:?}",
                    String::from_utf8_lossy(&rfc822[i.saturating_sub(40)..i + 1])
                );
            }
            if b == b'\r' && rfc822.get(i + 1) != Some(&b'\n') {
                panic!(
                    "bare CR at byte {i}: ...{:?}",
                    String::from_utf8_lossy(&rfc822[i.saturating_sub(40)..=i])
                );
            }
        }
    }

    #[test]
    fn reply_rfc822_with_crlf_quoted_html_has_no_bare_newlines() {
        // Mirrors a real contextual reply: the quoted parent HTML keeps its
        // CRLF line endings while Envelope's own glue joins with `\n`, and
        // non-ASCII content forces quoted-printable encoding. The CRLF+LF
        // boundary sequences must not serialize to bare newlines.
        let html = "<div class=\"envelope-agent-body\"><div>Repro body.</div></div>\n<br>\n\
                    <div class=\"gmail_quote\">\n  <div>On 2026-08-12, Ana María wrote:</div>\n  \
                    <blockquote class=\"gmail_quote\">\n<p>Hola señor Martin,</p>\r\n\
                    <p>segunda línea</p>\r\n\n  </blockquote>\n</div>";
        let (rfc822, _) = build_rfc822_full(
            "Agent <agent@example.com>",
            "a@example.com",
            "Re: mixed line endings",
            Some("Repro body.\n\n> Hola señor Martin,\n> segunda línea"),
            Some(html),
            None,
            Some("<parent@example.com>"),
            &[],
            None,
            &[],
        )
        .unwrap();
        assert_strict_crlf(&rfc822);
    }

    #[test]
    fn draft_rfc822_with_mixed_line_ending_body_has_no_bare_newlines() {
        // Draft create with a CRLF-terminated body that still mixes in bare
        // LFs (the reported repro: fully CRLF-terminated --body did not help).
        let (rfc822, _) = build_rfc822_draft(
            "agent@example.com",
            "a@example.com",
            Some("mixed endings"),
            Some("línea uno\r\n\nline two é\nline three\r\n"),
            None,
            None,
            None,
            &[],
        )
        .unwrap();
        assert_strict_crlf(&rfc822);
    }
}
