// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Shared Envelope attribution primitive.
//!
//! Envelope is the *host tool*; Governor is *blind attribution scoring*. This
//! module is the one place where Envelope honestly derives its own contextual
//! attributes from observable runtime/mailbox state and expresses them as the
//! canonical Governor **envelope** catalog attribute keys. Governor then scores
//! and routes those keys opaquely — Envelope never reconstructs weights,
//! thresholds, or scoring logic here.
//!
//! Two honesty rules are load-bearing:
//!
//! 1. **Classifiers emit booleans/classes only.** Message bodies, full recipient
//!    addresses, subjects, and attachment bytes never enter an
//!    [`AttributedSendContext`] and never appear in [`AttributedSendContext::to_governor_attrs`]
//!    output. The output is a set of compile-time catalog keys.
//! 2. **Unknown ⇒ omitted, never fabricated.** Signals Envelope cannot yet
//!    observe (PII / financial / legal / commitment / uncited classifiers) are
//!    represented as `Option<bool>` and left `None`, which omits the attribute.
//!    A fabricated `false` would understate risk and is never emitted.
//!
//! The derivation is pure (no I/O): callers gather facts from the store, draft
//! snapshot, send policy, and account, populate a context, and call
//! [`AttributedSendContext::to_governor_attrs`].

/// Observable, sanitized facts about an outbound send, mapped to canonical
/// Governor **envelope** catalog attribute keys via
/// [`AttributedSendContext::to_governor_attrs`].
///
/// Structural facts (reply/attachment/recipient/domain shape) are plain values
/// that are always observable at send time. Facts that require a store lookup or
/// a classifier that may not exist yet are `Option<bool>`: `Some(true)` emits the
/// attribute, `Some(false)` and `None` both omit it (the difference is only
/// whether Envelope *checked*).
#[derive(Debug, Clone, Default)]
pub struct AttributedSendContext {
    /// Lowercased sender domain (from the account username), if known.
    pub account_domain: Option<String>,
    /// Lowercased, de-duplicated recipient domains (never full addresses).
    pub recipient_domains: Vec<String>,
    /// Total recipient count across to/cc/bcc.
    pub recipient_count: usize,
    /// The send continues an existing thread (`In-Reply-To`/references present).
    pub is_reply: bool,
    /// A non-empty BCC field was used.
    pub has_bcc: bool,
    /// Number of attachments on the message.
    pub attachment_count: usize,
    /// At least one attachment classifies as contract/NDA/financial (class only).
    pub sensitive_attachment: bool,
    /// Configured trusted-domain allowlist (lowercased).
    pub trusted_domains: Vec<String>,
    /// The action only saves a draft; nothing is transmitted.
    pub draft_only: bool,
    /// A human explicitly approved this content (CLI/dashboard confirm). Never
    /// set from an agent self-approval.
    pub human_approved: bool,
    /// The draft was edited by a human after agent generation.
    pub human_edited: Option<bool>,
    /// The content was drafted by an AI agent (and not human-edited).
    pub agent_drafted: Option<bool>,
    /// The body is under 100 words (count only, never the content).
    pub short_body: Option<bool>,
    /// The recipient is a known contact (store lookup).
    pub known_contact: Option<bool>,
    /// The recipient exchanged 5+ messages in 30 days (store lookup).
    pub frequent_contact: Option<bool>,
    /// First-ever contact with this recipient (store history empty).
    pub cold_email: Option<bool>,
    /// The recipient domain has never been contacted (store history).
    pub unknown_domain: Option<bool>,
    /// The message is purely informational (heuristic classifier).
    pub informational: Option<bool>,
    /// Personally identifiable information detected (classifier — usually None).
    pub has_pii: Option<bool>,
    /// Money/invoice/payment language detected (classifier — usually None).
    pub financial_content: Option<bool>,
    /// Contract/IP/DMCA language detected (classifier — usually None).
    pub legal_content: Option<bool>,
    /// Promises/commitments on the human's behalf (classifier — usually None).
    pub commitment_language: Option<bool>,
    /// Factual claims without sources (classifier — usually None).
    pub uncited_claims: Option<bool>,
}

impl AttributedSendContext {
    /// Derive the canonical Governor **envelope** catalog attribute keys this
    /// context exhibits. The returned slice contains only compile-time catalog
    /// keys — never message content, recipient addresses, or secrets.
    pub fn to_governor_attrs(&self) -> Vec<&'static str> {
        let mut attrs: Vec<&'static str> = Vec::new();

        // ── Relationship ────────────────────────────────────────────────
        if self.is_reply {
            attrs.push("reply_to_thread");
        }
        push_if(&mut attrs, "known_contact", self.known_contact);
        push_if(&mut attrs, "frequent_contact", self.frequent_contact);
        push_if(&mut attrs, "cold_email", self.cold_email);

        // ── Domain ──────────────────────────────────────────────────────
        if self.all_recipients_internal() {
            attrs.push("internal_domain");
        }
        if self.any_trusted_domain() {
            attrs.push("trusted_domain");
        }
        push_if(&mut attrs, "unknown_domain", self.unknown_domain);
        if self.any_recipient_domain(is_freemail_domain) {
            attrs.push("freemail_domain");
        }
        if self.any_recipient_domain(is_disposable_domain) {
            attrs.push("disposable_domain");
        }
        if self.any_recipient_domain(is_gov_domain) {
            attrs.push("gov_domain");
        }

        // ── Intent ──────────────────────────────────────────────────────
        if self.draft_only {
            attrs.push("draft_only");
        }
        if self.human_approved {
            attrs.push("tyler_approved");
        }
        push_if(&mut attrs, "informational", self.informational);

        // ── Content ─────────────────────────────────────────────────────
        // A draft the human touched after generation is human-edited, not a raw
        // agent draft. The two are mutually exclusive; human editing wins.
        let human_edited = self.human_edited == Some(true);
        if human_edited {
            attrs.push("human_edited");
        } else {
            push_if(&mut attrs, "agent_drafted", self.agent_drafted);
        }
        push_if(&mut attrs, "short_body", self.short_body);
        if self.attachment_count > 0 {
            attrs.push("has_attachment");
        }
        if self.sensitive_attachment {
            attrs.push("sensitive_attachment");
        }
        push_if(&mut attrs, "has_pii", self.has_pii);
        push_if(&mut attrs, "uncited_claims", self.uncited_claims);

        // ── Stakes ──────────────────────────────────────────────────────
        push_if(&mut attrs, "financial_content", self.financial_content);
        push_if(&mut attrs, "legal_content", self.legal_content);
        push_if(&mut attrs, "commitment_language", self.commitment_language);

        // ── Recipient ───────────────────────────────────────────────────
        if self.recipient_count >= 6 {
            attrs.push("bulk_send");
        }
        if self.has_bcc {
            attrs.push("has_bcc");
        }

        attrs
    }

    /// True only when every recipient shares the sender's domain and there is at
    /// least one recipient.
    fn all_recipients_internal(&self) -> bool {
        match &self.account_domain {
            Some(acct) if !self.recipient_domains.is_empty() => {
                self.recipient_domains.iter().all(|d| d == acct)
            }
            _ => false,
        }
    }

    /// True when any recipient domain is on the configured trusted allowlist.
    fn any_trusted_domain(&self) -> bool {
        !self.trusted_domains.is_empty()
            && self
                .recipient_domains
                .iter()
                .any(|d| self.trusted_domains.iter().any(|t| t == d))
    }

    /// True when any recipient domain satisfies the classification predicate.
    fn any_recipient_domain(&self, pred: fn(&str) -> bool) -> bool {
        self.recipient_domains.iter().any(|d| pred(d))
    }
}

/// Push `key` only when the tri-state flag was observed *true*. `Some(false)` and
/// `None` both omit the attribute (honest omission over a fabricated `false`).
fn push_if(attrs: &mut Vec<&'static str>, key: &'static str, flag: Option<bool>) {
    if flag == Some(true) {
        attrs.push(key);
    }
}

/// A sanitized summary of a recipient header set: domains (no local parts), the
/// total address count, and whether a BCC was used.
#[derive(Debug, Clone, Default)]
pub struct RecipientSummary {
    pub domains: Vec<String>,
    pub count: usize,
    pub has_bcc: bool,
}

/// Parse to/cc/bcc header strings into a [`RecipientSummary`]. Full addresses are
/// never retained — only lowercased, de-duplicated domains, the count, and a BCC
/// flag.
pub fn collect_recipient_domains(
    to: &str,
    cc: Option<&str>,
    bcc: Option<&str>,
) -> RecipientSummary {
    let mut domains: Vec<String> = Vec::new();
    let mut count = 0usize;
    for header in [Some(to), cc, bcc].into_iter().flatten() {
        for token in header.split(',') {
            if let Some(domain) = recipient_domain(token) {
                count += 1;
                if !domains.contains(&domain) {
                    domains.push(domain);
                }
            }
        }
    }
    let has_bcc = bcc.is_some_and(|b| b.split(',').any(|t| recipient_domain(t).is_some()));
    RecipientSummary {
        domains,
        count,
        has_bcc,
    }
}

/// Extract a lowercased domain from a single recipient token, or `None` if the
/// token has no parseable address. Handles both `Name <a@b>` and bare `a@b`.
/// Never returns the full address.
pub fn recipient_domain(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let addr = if let Some(start) = token.rfind('<') {
        let end = token.rfind('>')?;
        if end <= start + 1 {
            return None;
        }
        token[start + 1..end].trim()
    } else {
        token
    };
    let normalized = addr.trim_matches('"').trim().to_ascii_lowercase();
    let (_local, domain) = normalized.split_once('@')?;
    if domain.is_empty() || domain.contains(' ') {
        return None;
    }
    Some(domain.to_string())
}

/// Classify a domain as a consumer freemail provider (observable membership, not
/// a scoring judgment).
pub fn is_freemail_domain(domain: &str) -> bool {
    const FREEMAIL: &[&str] = &[
        "gmail.com",
        "googlemail.com",
        "yahoo.com",
        "yahoo.co.uk",
        "ymail.com",
        "rocketmail.com",
        "hotmail.com",
        "hotmail.co.uk",
        "outlook.com",
        "live.com",
        "msn.com",
        "aol.com",
        "icloud.com",
        "me.com",
        "mac.com",
        "proton.me",
        "protonmail.com",
        "pm.me",
        "gmx.com",
        "gmx.de",
        "mail.com",
        "zoho.com",
        "yandex.com",
        "yandex.ru",
        "fastmail.com",
        "hey.com",
        "tutanota.com",
    ];
    let d = domain.trim().to_ascii_lowercase();
    FREEMAIL.contains(&d.as_str())
}

/// Classify a domain as a known disposable/throwaway email service.
pub fn is_disposable_domain(domain: &str) -> bool {
    const DISPOSABLE: &[&str] = &[
        "mailinator.com",
        "guerrillamail.com",
        "10minutemail.com",
        "tempmail.com",
        "temp-mail.org",
        "throwaway.email",
        "trashmail.com",
        "yopmail.com",
        "getnada.com",
        "nada.email",
        "sharklasers.com",
        "dispostable.com",
        "maildrop.cc",
        "fakeinbox.com",
        "mintemail.com",
        "mailnesia.com",
        "mohmal.com",
        "spamgourmet.com",
        "mailcatch.com",
        "emailondeck.com",
    ];
    let d = domain.trim().to_ascii_lowercase();
    DISPOSABLE.contains(&d.as_str())
}

/// Classify a domain as a government/military domain by well-known suffix.
pub fn is_gov_domain(domain: &str) -> bool {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    const GOV_SUFFIXES: &[&str] = &[
        ".gov", ".mil", ".gov.uk", ".mod.uk", ".gob.es", ".gob.mx", ".gouv.fr", ".go.jp",
        ".gov.au", ".gov.in", ".gov.br", ".gc.ca", ".gov.za", ".govt.nz", ".gov.sg",
    ];
    GOV_SUFFIXES.iter().any(|s| d.ends_with(s)) || d == "gov" || d == "mil"
}

/// Classify an attachment as sensitive (contract/NDA/financial) from its
/// filename. Class only — the attachment bytes are never inspected or retained
/// here. Envelope only *labels*; Governor decides what the label is worth.
pub fn classify_sensitive_attachment(filename: &str, _content_type: &str) -> bool {
    const MARKERS: &[&str] = &[
        "contract",
        "agreement",
        "nda",
        "non-disclosure",
        "invoice",
        "statement",
        "tax",
        "w-2",
        "w2",
        "1099",
        "payroll",
        "confidential",
        "financial",
        "wire",
        "settlement",
        "offer letter",
        "term sheet",
        "promissory",
    ];
    let name = filename.to_ascii_lowercase();
    MARKERS.iter().any(|m| name.contains(m))
}
