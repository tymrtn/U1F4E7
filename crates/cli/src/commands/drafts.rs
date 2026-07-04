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
use envelope_email_transport::imap;
use envelope_email_transport::outbound::SendSurface;
use envelope_email_transport::reply;
use envelope_email_transport::smtp::Attachment;
use envelope_email_transport::{detect_drafts_folder, detect_sent_folder};
use lettre::message::Mailboxes;
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as BuilderAddress;
use tracing::warn;

use super::attachments::{attachment_summaries, decode_attachments, snapshot_attachments};
use super::common::{resolve_account, setup_credentials};
use super::re_subject_guard::check_new_re_subject_guard;
use super::ui;

const DEFAULT_DASHBOARD_BASE_URL: &str = "http://localhost:3141";

fn dashboard_base_url() -> String {
    std::env::var("ENVELOPE_DASHBOARD_URL")
        .ok()
        .map(|url| url.trim().trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_DASHBOARD_BASE_URL.to_string())
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
    draft_dashboard_url_with_base(&dashboard_base_url(), account_id, draft_id)
}

/// Strip surrounding angle brackets from a Message-ID (`<id>` → `id`).
pub(crate) fn strip_brackets(s: &str) -> String {
    s.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

/// Return true when the SMTP provider is known to place submitted messages in
/// Sent Mail automatically.
///
/// Gmail does this for smtp.gmail.com. If Envelope also IMAP-APPENDs a second
/// copy after SMTP success, Gmail shows two sent messages: the provider's real
/// sent copy plus Envelope's manually appended local copy. Keep this deliberately
/// conservative; generic IMAP/SMTP providers still need Envelope's Sent append.
pub(crate) fn provider_auto_saves_sent(provider_type: Option<&str>, smtp_host: &str) -> bool {
    let provider = provider_type
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if matches!(provider.as_str(), "gmail" | "google" | "google_workspace") {
        return true;
    }

    let host = smtp_host.trim().to_ascii_lowercase();
    host == "smtp.gmail.com" || host.ends_with(".smtp.gmail.com") || host.contains("googlemail.com")
}

#[derive(Debug, Clone)]
pub(crate) struct SentMailProof {
    pub folder: Option<String>,
    pub uid: Option<u32>,
    pub lookup_status: &'static str,
    pub lookup_error: Option<String>,
    /// Stable label describing who created the Sent-folder copy.
    ///
    /// - `provider`: SMTP provider auto-filed the message (e.g. Gmail).
    /// - `client_appended`: Envelope IMAP-APPENDed an archive copy.
    /// - `unresolved`: Provider should auto-save but lookup hasn't found it yet.
    /// - `not_attempted`: No IMAP available; no copy confirmed.
    pub copy_source: &'static str,
}

impl SentMailProof {
    pub(crate) fn new(
        folder: Option<String>,
        uid: Option<u32>,
        lookup_status: &'static str,
        lookup_error: Option<String>,
    ) -> Self {
        Self {
            folder,
            uid,
            lookup_status,
            lookup_error,
            copy_source: "unresolved",
        }
    }

    pub(crate) fn message_url(&self, account_id: &str) -> Option<String> {
        let folder = self.folder.as_deref()?;
        let uid = self.uid?;
        ui::message_ui(account_id, uid, folder)
            .get("message_url")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    pub(crate) fn ui(&self, account_id: &str) -> serde_json::Value {
        match (self.folder.as_deref(), self.uid) {
            (Some(folder), Some(uid)) => ui::message_ui(account_id, uid, folder),
            _ => ui::account_ui(account_id),
        }
    }
}

/// Determine the stable `copy_source` label based on send-path context.
///
/// Arguments:
/// - `has_imap`: account has an IMAP host configured
/// - `provider_auto_saves`: provider is known to auto-file Sent (e.g. Gmail)
/// - `client_appended`: Envelope successfully IMAP-APPENDed an archive copy
/// - `lookup_found`: the post-send Sent-folder lookup found the message
#[cfg(test)]
pub(crate) fn determine_copy_source(
    has_imap: bool,
    provider_auto_saves: bool,
    client_appended: bool,
    lookup_found: bool,
) -> &'static str {
    if !has_imap {
        return "not_attempted";
    }
    if client_appended {
        return "client_appended";
    }
    if provider_auto_saves {
        if lookup_found {
            return "provider";
        } else {
            return "unresolved";
        }
    }
    // No IMAP append happened and provider doesn't auto-save — no copy confirmed.
    "not_attempted"
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

/// Decision produced by pre-append Sent-folder lookup semantics (issue #77).
#[derive(Debug, PartialEq)]
pub(crate) enum SentCopyDecision {
    /// Account has no IMAP — no copy possible.
    NoImap,
    /// Pre-send lookup found the message: provider already filed the copy.
    ProviderFound,
    /// Provider is known to auto-save but pre-send lookup missed it (timing).
    ProviderUnresolved,
    /// Provider does not auto-save and message not yet in Sent: client must append.
    NeedsClientAppend,
}

/// Pure function: determine the sent-copy action from IMAP availability, provider
/// auto-save flag, and whether the pre-append lookup found the message.
pub(crate) fn decide_sent_copy_action(
    has_imap: bool,
    provider_auto_saves: bool,
    pre_lookup_found: bool,
) -> SentCopyDecision {
    if !has_imap {
        return SentCopyDecision::NoImap;
    }
    if pre_lookup_found {
        return SentCopyDecision::ProviderFound;
    }
    if provider_auto_saves {
        return SentCopyDecision::ProviderUnresolved;
    }
    SentCopyDecision::NeedsClientAppend
}

/// Result of resolving the Sent-folder copy after SMTP success.
pub(crate) struct SentCopyResult {
    pub sent_mail_appended: bool,
    pub sent_mail_append_skipped_reason: Option<&'static str>,
    pub proof: SentMailProof,
}

pub(crate) async fn find_sent_mail_by_message_id(
    db: &Database,
    creds: &AccountWithCredentials,
    message_id: &str,
) -> SentMailProof {
    if message_id.trim().is_empty() {
        return SentMailProof::new(None, None, "no_message_id", None);
    }
    if creds.account.imap_host.trim().is_empty() {
        return SentMailProof::new(None, None, "no_imap", None);
    }

    let mut client = match imap::connect(creds).await {
        Ok(client) => client,
        Err(e) => {
            return SentMailProof::new(None, None, "imap_connect_failed", Some(e.to_string()));
        }
    };

    let sent_folder = match detect_sent_folder(&mut client, db, &creds.account.id).await {
        Ok(Some(folder)) => folder,
        Ok(None) => return SentMailProof::new(None, None, "sent_folder_not_found", None),
        Err(e) => {
            return SentMailProof::new(
                None,
                None,
                "sent_folder_detection_failed",
                Some(e.to_string()),
            );
        }
    };

    let mid_clean = message_id.trim_matches(|c| c == '<' || c == '>');
    let mut last_error: Option<String> = None;
    for attempt in 0..3 {
        match imap::find_uid_by_message_id(&mut client, &sent_folder, mid_clean).await {
            Ok(Some(uid)) => {
                return SentMailProof::new(Some(sent_folder), Some(uid), "found", None);
            }
            Ok(None) => {
                last_error = None;
            }
            Err(e) => {
                last_error = Some(e.to_string());
            }
        }
        if attempt < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    SentMailProof::new(
        Some(sent_folder),
        None,
        if last_error.is_some() {
            "lookup_failed"
        } else {
            "not_found"
        },
        last_error,
    )
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

    Ok((rfc822.into_bytes(), message_id))
}

/// After a successful immediate SMTP send, append a copy to the account's Sent
/// folder for providers that do not auto-save SMTP submissions.
///
/// Gmail/Google save submitted mail to `[Gmail]/Sent Mail` automatically, so we
/// skip them to avoid a visible duplicate. Generic IMAP/SMTP providers (martin.fm
/// / inbox.eu and friends) do not, so without this the message is never visible
/// in Sent and `find_sent_mail_by_message_id` can only ever report `not_found`.
///
/// The appended copy is rebuilt from the same fields and the *same* Message-ID
/// that was transmitted, so the subsequent proof lookup resolves it. This mirrors
/// the draft-send path's Sent-copy logic. Best-effort: connection/append failures
/// are logged and surfaced as not-appended rather than failing the send.
///
/// Returns `(appended, skipped_reason)`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn append_sent_copy_for_immediate_send(
    db: &Database,
    creds: &AccountWithCredentials,
    provider_type: Option<&str>,
    from: &str,
    to: &str,
    subject: &str,
    text: Option<&str>,
    html: Option<&str>,
    cc: Option<&str>,
    in_reply_to: Option<&str>,
    references: &[String],
    message_id: &str,
    attachments: &[Attachment],
) -> (bool, Option<&'static str>) {
    let acct = &creds.account;
    if acct.imap_host.trim().is_empty() {
        return (false, Some("no_imap"));
    }
    if provider_auto_saves_sent(provider_type, &acct.smtp_host) {
        return (false, Some("provider_auto_saves_sent"));
    }

    let rfc822 = match build_rfc822_full(
        from,
        to,
        subject,
        text,
        html,
        cc,
        in_reply_to,
        references,
        Some(message_id),
        attachments,
    ) {
        Ok((bytes, _)) => bytes,
        Err(e) => {
            warn!("failed to build Sent copy for immediate send: {e}");
            return (false, Some("rfc822_build_failed"));
        }
    };

    let mut client = match imap::connect(creds).await {
        Ok(client) => client,
        Err(e) => {
            warn!("failed to connect to IMAP to append Sent copy: {e}");
            return (false, Some("imap_connect_failed"));
        }
    };

    match detect_sent_folder(&mut client, db, &acct.id).await {
        Ok(Some(sent_folder)) => {
            match imap::append_message(&mut client, &sent_folder, "(\\Seen)", &rfc822).await {
                Ok(_) => (true, None),
                Err(e) => {
                    warn!("failed to append Sent copy to {sent_folder}: {e}");
                    (false, Some("append_failed"))
                }
            }
        }
        Ok(None) => (false, Some("sent_folder_not_found")),
        Err(e) => {
            warn!("failed to detect Sent folder for immediate send: {e}");
            (false, Some("sent_folder_detection_failed"))
        }
    }
}

/// After a successful SMTP send, determine the Sent-folder copy semantics using
/// a **pre-append** lookup.
///
/// Decision flow:
/// 1. No IMAP → `copy_source="not_attempted"`, skip everything.
/// 2. Pre-append lookup finds the message → `copy_source="provider"`, skip client append.
/// 3. Provider auto-saves but lookup missed → `copy_source="unresolved"`, skip client append.
/// 4. Provider does not auto-save and not found → client IMAP APPEND, then post-append
///    lookup; `copy_source="client_appended"` on append success.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_sent_copy_after_send(
    db: &Database,
    creds: &AccountWithCredentials,
    provider_type: Option<&str>,
    from: &str,
    to: &str,
    subject: &str,
    text: Option<&str>,
    html: Option<&str>,
    cc: Option<&str>,
    in_reply_to: Option<&str>,
    references: &[String],
    message_id: &str,
    attachments: &[Attachment],
) -> SentCopyResult {
    let has_imap = !creds.account.imap_host.trim().is_empty();
    let provider_auto_saves = provider_auto_saves_sent(provider_type, &creds.account.smtp_host);

    if !has_imap {
        let mut proof = SentMailProof::new(None, None, "no_imap", None);
        proof.copy_source = "not_attempted";
        return SentCopyResult {
            sent_mail_appended: false,
            sent_mail_append_skipped_reason: Some("no_imap"),
            proof,
        };
    }

    // Pre-append lookup: check whether the provider already filed the message.
    let pre_proof = find_sent_mail_by_message_id(db, creds, message_id).await;

    match decide_sent_copy_action(has_imap, provider_auto_saves, pre_proof.uid.is_some()) {
        SentCopyDecision::NoImap => unreachable!("has_imap checked above"),
        SentCopyDecision::ProviderFound => {
            let mut proof = pre_proof;
            proof.copy_source = "provider";
            SentCopyResult {
                sent_mail_appended: false,
                sent_mail_append_skipped_reason: Some("provider_auto_saves_sent"),
                proof,
            }
        }
        SentCopyDecision::ProviderUnresolved => {
            let mut proof = pre_proof;
            proof.copy_source = "unresolved";
            SentCopyResult {
                sent_mail_appended: false,
                sent_mail_append_skipped_reason: Some("provider_auto_saves_sent"),
                proof,
            }
        }
        SentCopyDecision::NeedsClientAppend => {
            let (appended, skip_reason) = append_sent_copy_for_immediate_send(
                db,
                creds,
                provider_type,
                from,
                to,
                subject,
                text,
                html,
                cc,
                in_reply_to,
                references,
                message_id,
                attachments,
            )
            .await;
            let mut proof = find_sent_mail_by_message_id(db, creds, message_id).await;
            proof.copy_source = if appended {
                "client_appended"
            } else {
                "not_attempted"
            };
            SentCopyResult {
                sent_mail_appended: appended,
                sent_mail_append_skipped_reason: skip_reason,
                proof,
            }
        }
    }
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

    Ok((rfc822.into_bytes(), message_id))
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

/// Delete a stale IMAP draft after a modify replaces it. Best-effort.
async fn delete_draft_best_effort(db: &Database, creds: &AccountWithCredentials, uid: u32) {
    if creds.account.imap_host.is_empty() {
        return;
    }
    let mut client = match imap::connect(creds).await {
        Ok(c) => c,
        Err(e) => {
            warn!("IMAP connect failed while replacing draft (old UID {uid} left in place): {e}");
            return;
        }
    };
    let folder = detect_drafts_folder(&mut client, db, &creds.account.id)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "Drafts".to_string());
    if let Err(e) = imap::delete_message(&mut client, &folder, uid).await {
        warn!("failed to delete replaced draft from IMAP {folder} (UID {uid}): {e}");
    }
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

    let (imap_synced, imap_folder, imap_uid) =
        append_draft_best_effort(db, creds, &rfc822, &message_id_hdr).await;

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
            "imap_synced": imap_synced,
            "imap_folder": if imap_synced { Some(imap_folder.clone()) } else { None },
            "local_only": !imap_synced,
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

    // Replace the IMAP draft: append the new RFC822, delete the stale copy.
    let old_uid = draft.imap_uid;
    let (imap_synced, imap_folder, new_uid) =
        append_draft_best_effort(db, creds, &rfc822, &message_id_hdr).await;
    if imap_synced && new_uid != old_uid {
        if let Some(stale) = old_uid {
            delete_draft_best_effort(db, creds, stale).await;
        }
    }

    db.update_draft_content(
        id,
        Some(&new_to),
        new_cc.as_deref(),
        bcc.or(draft.bcc_addr.as_deref()),
        Some(&new_subject),
        Some(&assembled.text),
        assembled.html.as_deref(),
    )
    .context("failed to update draft content")?;
    if let Some(uid) = new_uid {
        let _ = db.update_draft_imap_uid(id, uid);
    }
    db.update_draft_attachments(id, &attachment_snapshots)
        .context("failed to update draft attachments")?;

    // Update metadata: authored body + signature state + storage. Preserve
    // source/context/preview/references unchanged.
    let mut new_meta = meta.clone();
    if let Some(obj) = new_meta.as_object_mut() {
        obj.insert("agent_body_text".into(), serde_json::json!(agent_text));
        obj.insert("agent_body_html".into(), serde_json::json!(agent_html));
        obj.insert(
            "signature_applied".into(),
            serde_json::json!(assembled.signature_applied),
        );
        obj.insert(
            "storage".into(),
            serde_json::json!({
                "imap_synced": imap_synced,
                "imap_folder": if imap_synced { Some(imap_folder) } else { None },
                "local_only": !imap_synced,
            }),
        );
    }
    db.set_draft_metadata(id, &new_meta)
        .context("failed to update draft metadata")?;

    db.get_draft(id)
        .context("failed to reload draft")?
        .ok_or_else(|| anyhow::anyhow!("draft vanished after edit: {id}"))
}

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

    serde_json::json!({
        "status": "drafted",
        "draft_id": draft.id,
        "account_id": draft.account_id,
        "draft_kind": meta.get("draft_kind").and_then(|v| v.as_str()).unwrap_or("new"),
        "source": get("source"),
        "fields": {
            "to": draft.to_addr,
            "cc": draft.cc_addr,
            "bcc": draft.bcc_addr,
            "subject": draft.subject,
            "in_reply_to": get("in_reply_to"),
            "references": meta.get("references").cloned().unwrap_or_else(|| serde_json::json!([])),
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
                            "ui": ui::message_ui(&acct.id, s.uid, &drafts_folder),
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
    let has_imap = !creds.account.imap_host.is_empty();

    let mut imap_uid: Option<u32> = None;
    let mut imap_synced = false;
    let mut drafts_folder_name = String::from("Drafts");

    if has_imap {
        // ── IMAP-first: APPEND to the Drafts folder ──
        match imap::connect(&creds).await {
            Ok(mut client) => {
                // Detect the correct Drafts folder for this account
                let detected = detect_drafts_folder(&mut client, &db, &creds.account.id).await;
                match detected {
                    Ok(Some(folder)) => {
                        drafts_folder_name = folder.clone();
                        match imap::append_message(
                            &mut client,
                            &folder,
                            "(\\Draft \\Seen)",
                            &rfc822,
                        )
                        .await
                        {
                            Ok(()) => {
                                imap_synced = true;
                                // Try to find the UID of the appended message
                                if !message_id.is_empty() {
                                    let mid_clean =
                                        message_id.trim_matches(|c| c == '<' || c == '>');
                                    match imap::find_uid_by_message_id(
                                        &mut client,
                                        &folder,
                                        mid_clean,
                                    )
                                    .await
                                    {
                                        Ok(Some(uid)) => imap_uid = Some(uid),
                                        Ok(None) => warn!(
                                            "IMAP APPEND succeeded but could not find UID by Message-ID"
                                        ),
                                        Err(e) => {
                                            warn!("failed to search for appended draft UID: {e}")
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("IMAP APPEND to {folder} failed: {e}");
                            }
                        }
                    }
                    Ok(None) => {
                        warn!(
                            "no drafts folder detected for {}; saving locally only",
                            creds.account.username
                        );
                    }
                    Err(e) => {
                        warn!("drafts folder detection failed: {e}; saving locally only");
                    }
                }
            }
            Err(e) => {
                warn!("IMAP connect failed: {e}; saving draft locally only");
            }
        }
    } else {
        warn!(
            "account {} has no IMAP — draft saved locally only (send-only account)",
            creds.account.username
        );
    }

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
                "imap_synced": imap_synced,
                "imap_uid": imap_uid,
                "imap_folder": if imap_synced { Some(&drafts_folder_name) } else { None },
                "local_only": !imap_synced,
                "sync_status_reason": if !imap_synced && has_imap {
                    Some("imap_sync_failed")
                } else if !has_imap {
                    Some("no_imap")
                } else {
                    None
                },
                "dashboard_path": dashboard_path,
                "dashboard_url": dashboard_url,
                "review_url": dashboard_url,
                "metadata": {
                    "dashboard_path": dashboard_path,
                    "dashboard_url": dashboard_url,
                    "review_url": dashboard_url,
                },
                "warning": if !imap_synced && has_imap {
                    Some("IMAP sync failed — draft saved locally only. Retry with draft create or check IMAP connectivity.")
                } else if !has_imap {
                    Some("Send-only account (no IMAP) — draft is local only.")
                } else {
                    None
                },
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
        if imap_synced {
            if let Some(uid) = imap_uid {
                println!("  IMAP:    synced to {} (UID {})", drafts_folder_name, uid);
            } else {
                println!("  IMAP:    synced to {} (UID pending)", drafts_folder_name);
            }
        } else if has_imap {
            println!("  ⚠ IMAP:  sync failed — saved locally only");
        } else {
            println!("  ⚠ IMAP:  send-only account — saved locally only");
        }
    }

    Ok(())
}

// ─── draft send ──────────────────────────────────────────────────────────

#[tokio::main]
pub async fn run_send(
    id: &str,
    account: Option<&str>,
    json: bool,
    backend: CredentialBackend,
    cooldown_seconds: Option<i64>,
    send_now: bool,
    confirm_send_now: bool,
) -> Result<()> {
    use envelope_email_transport::outbound::{
        IMMEDIATE_SEND_CONFIRM_CODE, SendDisposition, resolve_cooldown_seconds, resolve_disposition,
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
            let draft = db
                .get_draft(id)
                .context("failed to get draft")?
                .ok_or_else(|| anyhow::anyhow!("draft not found: {id}"))?;
            let send_at = (chrono::Utc::now() + chrono::Duration::seconds(cd))
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            db.update_draft_send_after(&draft.id, &send_at)
                .context("failed to set send_after on draft")?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "scheduled",
                        "draft_id": draft.id,
                        "send_after": send_at,
                        "cooldown_seconds": cd,
                        "ui": ui::draft_ui(&draft.account_id, &draft.id),
                    })
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

    let outcome = send_existing_draft(id, account, backend).await?;
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

/// Send an already-created draft (by local UUID or IMAP UID) without printing
/// anything. This is the single source of truth for "send this draft": it sends
/// over SMTP, cleans up the IMAP Drafts copy, optionally appends to Sent, and —
/// critically — marks the local draft row as sent so the local DB can never be
/// left at `status=draft` with no `sent_at` after a successful send.
pub(crate) async fn send_existing_draft(
    id: &str,
    account: Option<&str>,
    backend: CredentialBackend,
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

    // Threading (In-Reply-To / References) + preserved Message-ID from the
    // local draft metadata — preferred so reply headers survive the send.
    let (meta_in_reply_to, meta_references, _meta_message_id) = local_draft
        .as_ref()
        .map(|d| threading_from_metadata(d.metadata.as_ref()))
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
    {
        let gov_req = super::governor_gate::governor_request(
            &acct.id,
            super::governor_gate::account_domain(&creds.account.username),
            &subject,
            &to_addr,
            cc_addr.as_deref(),
            bcc_addr.as_deref(),
            SendSurface::Cli,
            Some(id),
            &attachments,
            in_reply_to.is_some(),
        );
        let gov_outcome = super::governor_gate::gate_and_record(&db, &acct.id, &gov_req);
        if !gov_outcome.allowed {
            bail!(
                "send blocked by governor: {} ({})",
                gov_outcome
                    .block_reason
                    .clone()
                    .unwrap_or_else(|| "governor did not permit this send".to_string()),
                gov_outcome
                    .block_code
                    .clone()
                    .unwrap_or_else(|| "governor_blocked".to_string())
            );
        }
    }

    // ── Send via SMTP (full path so In-Reply-To / References survive) ──
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

    // ── Delete from IMAP Drafts folder ──
    // Only attempt when the draft had an IMAP UID (otherwise there's nothing to delete).
    if let Some(uid) = imap_uid {
        if !acct.imap_host.is_empty() {
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
                    warn!("failed to connect to IMAP to clean up sent draft: {e}");
                }
            }
        }
    }

    // ── Update local SQLite record ──
    if local_draft.is_some() {
        db.mark_draft_sent(id, Some(&message_id))
            .context("failed to mark draft as sent")?;
    }

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
        in_reply_to.as_deref(),
        &references,
        &message_id,
        &attachments,
    )
    .await;

    let sent_mail_appended = copy_result.sent_mail_appended;
    let sent_mail_append_skipped_reason = copy_result.sent_mail_append_skipped_reason;
    let sent_mail_proof = copy_result.proof;

    let provider_sent_copy = if matches!(sent_mail_proof.copy_source, "provider" | "unresolved") {
        Some(sent_mail_proof_json(&acct.id, &sent_mail_proof))
    } else {
        None
    };
    let client_appended_copy = if sent_mail_proof.copy_source == "client_appended" {
        Some(sent_mail_proof_json(&acct.id, &sent_mail_proof))
    } else {
        None
    };

    let sent_message_url = sent_mail_proof.message_url(&acct.id);
    let sent_ui = sent_mail_proof.ui(&acct.id);

    let json = serde_json::json!({
        "status": "sent",
        "draft_id": id,
        "to": to_addr.clone(),
        "subject": subject.clone(),
        "message_id": message_id.clone(),
        "imap_draft_deleted": imap_uid.is_some(),
        "sent_mail_appended": sent_mail_appended,
        "sent_mail_append_skipped_reason": sent_mail_append_skipped_reason,
        "sent_folder": sent_mail_proof.folder.clone(),
        "sent_uid": sent_mail_proof.uid,
        "sent_message_url": sent_message_url.clone(),
        "sent_mail": sent_mail_proof_json(&acct.id, &sent_mail_proof),
        "provider_sent_copy": provider_sent_copy,
        "client_appended_copy": client_appended_copy,
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
                .contains("/messages/42")
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
        assert_eq!(determine_copy_source(true, true, false, true), "provider");
    }

    #[test]
    fn copy_source_is_unresolved_when_auto_saved_but_lookup_not_found() {
        assert_eq!(
            determine_copy_source(true, true, false, false),
            "unresolved"
        );
    }

    #[test]
    fn copy_source_is_client_appended_when_client_wrote_archive_and_lookup_found() {
        assert_eq!(
            determine_copy_source(true, false, true, true),
            "client_appended"
        );
    }

    #[test]
    fn copy_source_is_client_appended_when_append_done_but_lookup_not_found() {
        // Lookup may still be delayed; source is still client_appended.
        assert_eq!(
            determine_copy_source(true, false, true, false),
            "client_appended"
        );
    }

    #[test]
    fn copy_source_is_not_attempted_when_no_imap() {
        assert_eq!(
            determine_copy_source(false, false, false, false),
            "not_attempted"
        );
    }

    #[test]
    fn copy_source_not_attempted_overrides_provider_auto_saves_when_no_imap() {
        // No IMAP means we couldn't verify provider copy either.
        assert_eq!(
            determine_copy_source(false, true, false, false),
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

    // ─── decide_sent_copy_action (issue #77 pre-lookup semantics) ────────────

    #[test]
    fn decide_sent_copy_no_imap_always_returns_no_imap() {
        assert_eq!(
            decide_sent_copy_action(false, false, false),
            SentCopyDecision::NoImap
        );
        assert_eq!(
            decide_sent_copy_action(false, true, true),
            SentCopyDecision::NoImap
        );
    }

    #[test]
    fn decide_sent_copy_pre_lookup_found_means_provider_copy() {
        // Pre-lookup found message before any append → provider placed it.
        assert_eq!(
            decide_sent_copy_action(true, false, true),
            SentCopyDecision::ProviderFound
        );
        assert_eq!(
            decide_sent_copy_action(true, true, true),
            SentCopyDecision::ProviderFound
        );
    }

    #[test]
    fn decide_sent_copy_auto_saves_but_lookup_missed_is_unresolved() {
        assert_eq!(
            decide_sent_copy_action(true, true, false),
            SentCopyDecision::ProviderUnresolved
        );
    }

    #[test]
    fn decide_sent_copy_no_auto_save_and_not_found_needs_client_append() {
        assert_eq!(
            decide_sent_copy_action(true, false, false),
            SentCopyDecision::NeedsClientAppend
        );
    }
}
