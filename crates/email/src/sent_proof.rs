// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Source-aware Sent-folder proof resolution shared by every real SMTP path.
//!
//! After an SMTP send is accepted, Envelope resolves a truthful record of the
//! message's Sent-folder copy: it looks the message up in the provider's Sent
//! folder by Message-ID, and only client-APPENDs an archive copy when the
//! provider is not known to auto-file submitted mail and the message is not
//! already there. The resulting [`SentMailProof`] carries a stable `copy_source`
//! label so a client-appended copy is *never* reported as provider proof.
//!
//! This is the single implementation used by BOTH the immediate CLI/MCP send
//! paths and the background scheduled-send sweep. It lives in the transport
//! crate (not the CLI) precisely so the sweep — which lives in the dashboard
//! crate and cannot call back into the CLI — resolves Sent copies identically to
//! an immediate send. The archive copy is rebuilt with the SAME builder the SMTP
//! send used ([`crate::smtp::build_message`]) and the SAME Message-ID that was
//! transmitted, so the archived copy carries the same To/Cc, subject, text/HTML,
//! attachments, threading headers (In-Reply-To/References), and `Reply-To` that
//! were sent. `Bcc` is additionally kept on this sender-private archive (via
//! `keep_bcc`) so the sender retains the true recipient record — normal sends
//! still strip it from the wire. The subsequent proof lookup then resolves the
//! copy by an exact, unique Message-ID match.

use envelope_email_store::Database;
use envelope_email_store::models::AccountWithCredentials;
use tracing::warn;

use crate::folders::detect_sent_folder;
use crate::smtp::Attachment;

/// A truthful record of a sent message's Sent-folder copy.
#[derive(Debug, Clone)]
pub struct SentMailProof {
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
    pub fn new(
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
}

/// Result of resolving the Sent-folder copy after SMTP success.
pub struct SentCopyResult {
    pub sent_mail_appended: bool,
    pub sent_mail_append_skipped_reason: Option<&'static str>,
    pub proof: SentMailProof,
}

/// Return true when the SMTP provider is known to place submitted messages in
/// Sent Mail automatically.
///
/// Gmail does this for smtp.gmail.com. If Envelope also IMAP-APPENDs a second
/// copy after SMTP success, Gmail shows two sent messages: the provider's real
/// sent copy plus Envelope's manually appended local copy. Keep this deliberately
/// conservative; generic IMAP/SMTP providers still need Envelope's Sent append.
pub fn provider_auto_saves_sent(provider_type: Option<&str>, smtp_host: &str) -> bool {
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

/// Determine the stable `copy_source` label, coherent with the actually
/// observed/attempted outcome (no impossible source/UID pairs such as
/// `not_attempted` alongside a resolved UID).
///
/// Inputs:
/// - `has_imap`: account has an IMAP host configured
/// - `provider_auto_saves`: provider is known to auto-file Sent (e.g. Gmail)
/// - `append_attempted`: Envelope attempted a client IMAP APPEND
/// - `client_appended`: that client APPEND succeeded
/// - `lookup_found_unique`: a post-send lookup found exactly one exact copy
///
/// Precedence (each arm is the real observed state):
/// - no IMAP → `not_attempted` (no Sent operation was possible);
/// - client APPEND succeeded → `client_appended` (even if a later unique-UID
///   lookup cannot resolve — e.g. a duplicate now exists);
/// - an exact provider-side copy is actually present → `provider` (provider
///   auto-save, or a provider race after a failed append: label the observed
///   copy, never client-appended);
/// - provider is known to auto-save but the lookup missed it → `unresolved`;
/// - a client APPEND was attempted but failed and no exact copy appears →
///   `unresolved` (an attempt was made — never `not_attempted`);
/// - otherwise no Sent operation was attempted → `not_attempted`.
pub fn determine_copy_source(
    has_imap: bool,
    provider_auto_saves: bool,
    append_attempted: bool,
    client_appended: bool,
    lookup_found_unique: bool,
) -> &'static str {
    if !has_imap {
        return "not_attempted";
    }
    if client_appended {
        return "client_appended";
    }
    if lookup_found_unique {
        return "provider";
    }
    if provider_auto_saves {
        return "unresolved";
    }
    if append_attempted {
        return "unresolved";
    }
    "not_attempted"
}

/// Decision produced by pre-append Sent-folder lookup semantics (issue #77).
#[derive(Debug, PartialEq)]
pub enum SentCopyDecision {
    /// Account has no IMAP — no copy possible.
    NoImap,
    /// Pre-send lookup found exactly one exact copy: provider already filed it.
    ProviderFound,
    /// Provider is known to auto-save but the definitive lookup missed it (timing).
    ProviderUnresolved,
    /// The pre-append lookup was inconclusive, so Envelope cannot prove the
    /// message is absent and must NOT append another archive: multiple exact
    /// copies already exist (`ambiguous_sent_copies`), or a lookup/connect/
    /// detection failure left absence unproven (`sent_lookup_inconclusive`). The
    /// wrapped string is the stable skipped reason.
    Unresolved(&'static str),
    /// Provider does not auto-save and the message is definitively not yet in
    /// Sent: client must append an archive copy.
    NeedsClientAppend,
}

/// Pure function: determine the sent-copy action from IMAP availability, the
/// provider auto-save flag, and the exact-match `lookup_status` produced by
/// [`find_sent_mail_by_message_id`].
///
/// The pre-append lookup's classification is preserved rather than collapsed to
/// "did a UID come back", so ambiguous duplicates and inconclusive failures never
/// route to a client APPEND:
/// - no IMAP → [`SentCopyDecision::NoImap`];
/// - `found` (exact unique) → [`SentCopyDecision::ProviderFound`], no append;
/// - `ambiguous` (multiple exact copies) → [`SentCopyDecision::Unresolved`]
///   (`ambiguous_sent_copies`), no append;
/// - `not_found` (definitive absence) + auto-save provider →
///   [`SentCopyDecision::ProviderUnresolved`], no append (provider race);
/// - `not_found` + generic provider → [`SentCopyDecision::NeedsClientAppend`];
/// - any other status (lookup/connect/detection failure, or otherwise
///   inconclusive) → [`SentCopyDecision::Unresolved`] (`sent_lookup_inconclusive`),
///   no append — safer than appending when absence could not be proven.
pub fn decide_sent_copy_action(
    has_imap: bool,
    provider_auto_saves: bool,
    lookup_status: &str,
) -> SentCopyDecision {
    if !has_imap {
        return SentCopyDecision::NoImap;
    }
    match lookup_status {
        "found" => SentCopyDecision::ProviderFound,
        "ambiguous" => SentCopyDecision::Unresolved("ambiguous_sent_copies"),
        "not_found" => {
            if provider_auto_saves {
                SentCopyDecision::ProviderUnresolved
            } else {
                SentCopyDecision::NeedsClientAppend
            }
        }
        _ => SentCopyDecision::Unresolved("sent_lookup_inconclusive"),
    }
}

/// Look up a message in the account's Sent folder by Message-ID.
///
/// Read-only, best-effort, and content-free: connects IMAP, detects the Sent
/// folder, and resolves the message by an **exact, unique** Message-ID match
/// (with a small retry). IMAP `SEARCH HEADER` is substring-based, so hits are
/// treated as candidates whose actual Message-ID headers are fetched
/// (`BODY.PEEK`, no flag changes) and compared exactly after normalization. A
/// UID is returned only when exactly one exact match exists; multiple exact
/// matches yield a stable `ambiguous` status with a null UID. Any no-IMAP /
/// connect / detection / lookup failure likewise yields a proof with a stable
/// `lookup_status` and a null UID — never a fabricated or arbitrary copy.
pub async fn find_sent_mail_by_message_id(
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

    let mut client = match crate::imap::connect(creds).await {
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

    let mut last_error: Option<String> = None;
    for attempt in 0..3 {
        match crate::imap::find_exact_message_id_match(&mut client, &sent_folder, message_id).await
        {
            Ok(crate::imap::ExactMessageIdMatch::Unique(uid)) => {
                return SentMailProof::new(Some(sent_folder), Some(uid), "found", None);
            }
            Ok(crate::imap::ExactMessageIdMatch::Ambiguous) => {
                // Duplicate Message-IDs in the Sent folder: identity is
                // ambiguous, so return a stable status with no arbitrary UID
                // rather than pointing at one of several copies. Retrying cannot
                // disambiguate, so stop here.
                return SentMailProof::new(Some(sent_folder), None, "ambiguous", None);
            }
            Ok(crate::imap::ExactMessageIdMatch::None) => {
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

/// After a successful SMTP send, append an archive copy to the account's Sent
/// folder for providers that do not auto-save SMTP submissions.
///
/// The appended copy is rebuilt with [`crate::smtp::build_message`] — the SAME
/// builder the SMTP send used — and the SAME Message-ID that was transmitted, so
/// To/Cc, subject, text/HTML, attachments, threading headers, and `Reply-To` are
/// preserved and the subsequent proof lookup resolves it. `Bcc` is kept on this
/// sender-private archive (via `keep_bcc`) so the sender retains the true
/// recipient record; normal sends still strip it from the wire. Gmail/Google
/// save submitted mail automatically, so they are skipped to avoid a visible
/// duplicate. Best-effort: connection/append failures are logged and surfaced as
/// not-appended rather than failing the send.
///
/// Returns `(appended, skipped_reason)`.
#[allow(clippy::too_many_arguments)]
async fn append_sent_copy(
    db: &Database,
    creds: &AccountWithCredentials,
    provider_type: Option<&str>,
    from: &str,
    to: &str,
    subject: &str,
    text: Option<&str>,
    html: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    reply_to: Option<&str>,
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

    // Rebuild the exact message that was sent, preserving the transmitted
    // Message-ID. A non-empty `from` is used as the From override so the archive
    // copy matches the send; an empty `from` falls back to the account default
    // (what a scheduled send transmits).
    let bare_mid = message_id.trim_matches(|c| c == '<' || c == '>');
    let from_override = (!from.trim().is_empty()).then_some(from);
    let references_opt = if references.is_empty() {
        None
    } else {
        Some(references)
    };
    let rfc822 = match crate::smtp::build_message(
        creds,
        bare_mid,
        to,
        subject,
        text,
        html,
        from_override,
        cc,
        bcc,
        true, // keep_bcc: this is the sender-private Sent archive, not a wire send
        reply_to,
        in_reply_to,
        references_opt,
        attachments,
    ) {
        // IMAP APPEND requires strict CRLF; normalize the composed copy so a
        // body with mixed line endings can't be rejected as "bare newlines".
        Ok((email, _)) => crate::compose::normalize_crlf(&email.formatted()),
        Err(e) => {
            warn!("failed to build Sent copy for send: {e}");
            return (false, Some("rfc822_build_failed"));
        }
    };

    let mut client = match crate::imap::connect(creds).await {
        Ok(client) => client,
        Err(e) => {
            warn!("failed to connect to IMAP to append Sent copy: {e}");
            return (false, Some("imap_connect_failed"));
        }
    };

    match detect_sent_folder(&mut client, db, &acct.id).await {
        Ok(Some(sent_folder)) => {
            match crate::imap::append_message(&mut client, &sent_folder, "(\\Seen)", &rfc822).await
            {
                Ok(_) => (true, None),
                Err(e) => {
                    warn!("failed to append Sent copy to {sent_folder}: {e}");
                    (false, Some("append_failed"))
                }
            }
        }
        Ok(None) => (false, Some("sent_folder_not_found")),
        Err(e) => {
            warn!("failed to detect Sent folder for send: {e}");
            (false, Some("sent_folder_detection_failed"))
        }
    }
}

/// After a successful SMTP send, determine the Sent-folder copy semantics using
/// a **pre-append** lookup.
///
/// Decision flow (the pre-append lookup's exact-match classification is preserved,
/// never collapsed to "did a UID come back"):
/// 1. No IMAP → `copy_source="not_attempted"`, skip everything.
/// 2. Pre-append lookup finds exactly one exact copy → `copy_source="provider"`, skip client append.
/// 3. Provider auto-saves but definitive lookup missed → `copy_source="unresolved"`, skip client append.
/// 4. Ambiguous duplicates or an inconclusive lookup (connect/detection/lookup
///    failure) → `copy_source="unresolved"`, skip client append with a specific
///    reason (`ambiguous_sent_copies` / `sent_lookup_inconclusive`); Envelope
///    never appends when it could not prove the message is absent.
/// 5. Provider does not auto-save and definitively not found → client IMAP APPEND,
///    then post-append lookup; `copy_source="client_appended"` on append success.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_sent_copy_after_send(
    db: &Database,
    creds: &AccountWithCredentials,
    provider_type: Option<&str>,
    from: &str,
    to: &str,
    subject: &str,
    text: Option<&str>,
    html: Option<&str>,
    cc: Option<&str>,
    bcc: Option<&str>,
    reply_to: Option<&str>,
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

    match decide_sent_copy_action(has_imap, provider_auto_saves, pre_proof.lookup_status) {
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
        SentCopyDecision::Unresolved(reason) => {
            // Ambiguous duplicates or an inconclusive lookup: Envelope could not
            // prove the message is absent, so it never appends another archive.
            // Preserve the pre-lookup classification (e.g. `ambiguous`, null UID)
            // and surface a specific skipped reason.
            let mut proof = pre_proof;
            proof.copy_source = "unresolved";
            SentCopyResult {
                sent_mail_appended: false,
                sent_mail_append_skipped_reason: Some(reason),
                proof,
            }
        }
        SentCopyDecision::NeedsClientAppend => {
            let (appended, skip_reason) = append_sent_copy(
                db,
                creds,
                provider_type,
                from,
                to,
                subject,
                text,
                html,
                cc,
                bcc,
                reply_to,
                in_reply_to,
                references,
                message_id,
                attachments,
            )
            .await;
            // Post-append lookup by exact, unique Message-ID. The label is
            // coherent with what actually happened: a successful append is
            // `client_appended` even if a unique UID cannot be resolved; a failed
            // append that nonetheless finds one exact copy (provider race) is
            // labeled by that observed provider-side copy; a failed append with
            // no exact copy is `unresolved` (an attempt was made — never
            // `not_attempted`).
            let mut proof = find_sent_mail_by_message_id(db, creds, message_id).await;
            proof.copy_source = determine_copy_source(
                true, // has_imap: checked above
                provider_auto_saves,
                true, // append_attempted
                appended,
                proof.uid.is_some(),
            );
            SentCopyResult {
                sent_mail_appended: appended,
                sent_mail_append_skipped_reason: skip_reason,
                proof,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::models::Account;

    fn local_only_creds() -> AccountWithCredentials {
        // No IMAP host: a local-only account never confirms a Sent copy, and the
        // resolver must return immediately without opening a socket.
        let account = Account {
            id: "acct-test".to_string(),
            name: "Test".to_string(),
            username: "op@example.com".to_string(),
            domain: "example.com".to_string(),
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            imap_host: String::new(),
            imap_port: 993,
            smtp_username: None,
            imap_username: None,
            display_name: None,
            signature_text: None,
            signature_html: None,
            created_at: String::new(),
        };
        AccountWithCredentials {
            account,
            password: "unused".to_string(),
            smtp_password: None,
            imap_password: None,
        }
    }

    #[test]
    fn decide_action_covers_the_pre_append_matrix() {
        // No IMAP → nothing possible, regardless of provider/lookup_status.
        assert_eq!(
            decide_sent_copy_action(false, false, "not_found"),
            SentCopyDecision::NoImap
        );
        assert_eq!(
            decide_sent_copy_action(false, true, "found"),
            SentCopyDecision::NoImap
        );

        // Exact unique copy already present → provider-side proof, no append
        // (either provider class).
        assert_eq!(
            decide_sent_copy_action(true, true, "found"),
            SentCopyDecision::ProviderFound
        );
        assert_eq!(
            decide_sent_copy_action(true, false, "found"),
            SentCopyDecision::ProviderFound
        );

        // Definitive absence: auto-save provider races (copy still coming) →
        // unresolved; generic provider must client-append.
        assert_eq!(
            decide_sent_copy_action(true, true, "not_found"),
            SentCopyDecision::ProviderUnresolved
        );
        assert_eq!(
            decide_sent_copy_action(true, false, "not_found"),
            SentCopyDecision::NeedsClientAppend
        );
    }

    #[test]
    fn decide_action_ambiguous_never_appends_for_any_provider() {
        // Multiple exact copies already exist: identity is ambiguous, so Envelope
        // must never APPEND another archive on top of duplicates — for either
        // provider class. This is the regression proving ambiguity never reaches
        // NeedsClientAppend / append_sent_copy.
        for auto_saves in [false, true] {
            let decision = decide_sent_copy_action(true, auto_saves, "ambiguous");
            assert_eq!(
                decision,
                SentCopyDecision::Unresolved("ambiguous_sent_copies"),
                "ambiguous pre-lookup must be unresolved, never an append"
            );
            assert_ne!(decision, SentCopyDecision::NeedsClientAppend);
        }
    }

    #[test]
    fn decide_action_inconclusive_lookups_are_unresolved_never_append() {
        // Any lookup/connect/detection failure or otherwise inconclusive status
        // means Envelope could not prove the message is absent. Appending would
        // risk a duplicate, so these are unresolved with no append — even for a
        // generic (non-auto-save) provider that would otherwise append.
        for status in [
            "lookup_failed",
            "imap_connect_failed",
            "sent_folder_detection_failed",
            "sent_folder_not_found",
            "no_message_id",
        ] {
            for auto_saves in [false, true] {
                let decision = decide_sent_copy_action(true, auto_saves, status);
                assert_eq!(
                    decision,
                    SentCopyDecision::Unresolved("sent_lookup_inconclusive"),
                    "inconclusive status {status} must not append"
                );
                assert_ne!(decision, SentCopyDecision::NeedsClientAppend);
            }
        }
    }

    #[test]
    fn determine_copy_source_matrix_is_coherent_with_the_observed_outcome() {
        // Args: (has_imap, provider_auto_saves, append_attempted, client_appended,
        //        lookup_found_unique).

        // No IMAP → never any Sent operation.
        assert_eq!(
            determine_copy_source(false, false, false, false, false),
            "not_attempted"
        );
        assert_eq!(
            determine_copy_source(false, true, false, false, true),
            "not_attempted"
        );

        // Pre-send provider copy actually found (auto-save) → provider.
        assert_eq!(
            determine_copy_source(true, true, false, false, true),
            "provider"
        );
        // Auto-save provider but lookup missed → unresolved (never not_attempted).
        assert_eq!(
            determine_copy_source(true, true, false, false, false),
            "unresolved"
        );

        // Client append succeeded → client_appended, even if a unique UID cannot
        // be resolved (a duplicate now exists).
        assert_eq!(
            determine_copy_source(true, false, true, true, true),
            "client_appended"
        );
        assert_eq!(
            determine_copy_source(true, false, true, true, false),
            "client_appended"
        );

        // Append attempted but FAILED, no exact copy appears → unresolved (an
        // attempt was made — the old code wrongly said not_attempted here).
        assert_eq!(
            determine_copy_source(true, false, true, false, false),
            "unresolved"
        );
        // Append FAILED but exactly one provider-side copy appears (provider
        // race) → labeled by the observed provider copy, never client_appended.
        assert_eq!(
            determine_copy_source(true, false, true, false, true),
            "provider"
        );

        // IMAP present but no Sent operation attempted at all → not_attempted.
        assert_eq!(
            determine_copy_source(true, false, false, false, false),
            "not_attempted"
        );
    }

    #[test]
    fn provider_auto_saves_detects_gmail_only() {
        assert!(provider_auto_saves_sent(Some("gmail"), ""));
        assert!(provider_auto_saves_sent(None, "smtp.gmail.com"));
        assert!(!provider_auto_saves_sent(Some("generic"), "smtp.martin.fm"));
        assert!(!provider_auto_saves_sent(None, "mail.example.com"));
    }

    #[tokio::test]
    async fn resolve_on_a_local_only_account_is_not_attempted_without_network() {
        // A queued draft on an IMAP-less account: the resolver must return
        // `not_attempted` with no UID and never open a socket. This exercises the
        // exact helper the scheduled sweep reaches, offline.
        let db = Database::open_memory().unwrap();
        let creds = local_only_creds();
        let result = resolve_sent_copy_after_send(
            &db,
            &creds,
            None,
            "op@example.com",
            "to@example.com",
            "Subject",
            Some("a short body"),
            None, // html
            None, // cc
            None, // bcc
            None, // reply_to
            None, // in_reply_to
            &[],
            "<mid@example.com>",
            &[],
        )
        .await;
        assert!(!result.sent_mail_appended);
        assert_eq!(result.sent_mail_append_skipped_reason, Some("no_imap"));
        assert_eq!(result.proof.copy_source, "not_attempted");
        assert_eq!(result.proof.uid, None);
        assert_eq!(result.proof.lookup_status, "no_imap");
    }
}
