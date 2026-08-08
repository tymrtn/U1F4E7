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
//!
//! ## Attribution resolution
//!
//! [`resolve`] is the trust-boundary gate that turns a bot's *declared*
//! attributes plus Envelope's *derived* facts into the validated *governor* set
//! actually submitted for scoring. It enforces the non-negotiable rules:
//! every bot-originated send must carry at least one factual declaration (host
//! facts never substitute); unknown/attestation-only/contradicting/impossible
//! declarations reject the whole request before Governor is ever spawned; and no
//! bad key is ever silently dropped or accepted.

use crate::attribution_provenance::{Provenance, conflicting_partner, provenance_of};
use crate::governor_catalog::{catalog_version, nearest_keys};

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
        // AI origin and later human editing can BOTH be true — they may
        // intentionally offset one another. Per the operator-reviewed design the
        // protocol does not force them mutually exclusive; each is emitted on its
        // own observed truth, and any exclusivity is a future catalog decision.
        push_if(&mut attrs, "human_edited", self.human_edited);
        push_if(&mut attrs, "agent_drafted", self.agent_drafted);
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

    /// Envelope's **tri-state** observation of a host-derived catalog key at send
    /// time:
    ///
    /// - `Some(true)` — Envelope independently observed the key present. A bot
    ///   declaration of it is corroborated and accepted as `accepted_redundant`
    ///   (it *counts*, because the bot actually declared it AND the host agrees).
    /// - `Some(false)` — Envelope observed the key absent. A declaration
    ///   contradicts the host and is rejected `conflicts_with_host_observation`.
    /// - `None` — Envelope could not observe the key (a store fact it did not look
    ///   up, or a host-derived key it has no signal for). A declaration cannot be
    ///   corroborated, so it is rejected `host_verification_unavailable` — never
    ///   silently accepted. Host facts alone never substitute for a real
    ///   declaration; an *unverifiable* claimed one does not either.
    ///
    /// Every key in [`crate::attribution_provenance::HOST_DERIVED`] resolves here
    /// to one of the three states; the catch-all `None` means an unhandled
    /// host-derived key fails closed (unverifiable) rather than being accepted.
    pub fn observe_host_key(&self, key: &str) -> Option<bool> {
        match key {
            // Authoritative structural facts, always observable at send time.
            "reply_to_thread" => Some(self.is_reply),
            "has_attachment" => Some(self.attachment_count > 0),
            "sensitive_attachment" => Some(self.sensitive_attachment),
            "has_bcc" => Some(self.has_bcc),
            "bulk_send" => Some(self.recipient_count >= 6),
            "draft_only" => Some(self.draft_only),
            // An actual send is never a read-only / folder / delete op.
            "read_only" | "move_to_folder" | "delete_message" => Some(false),
            // Domain-shape facts: observable only when we actually have recipient
            // domains (and, for internal, the sender domain).
            "internal_domain" => {
                if self.account_domain.is_some() && !self.recipient_domains.is_empty() {
                    Some(self.all_recipients_internal())
                } else {
                    None
                }
            }
            "trusted_domain" => {
                (!self.recipient_domains.is_empty()).then(|| self.any_trusted_domain())
            }
            "freemail_domain" => (!self.recipient_domains.is_empty())
                .then(|| self.any_recipient_domain(is_freemail_domain)),
            "disposable_domain" => (!self.recipient_domains.is_empty())
                .then(|| self.any_recipient_domain(is_disposable_domain)),
            "gov_domain" => (!self.recipient_domains.is_empty())
                .then(|| self.any_recipient_domain(is_gov_domain)),
            // Store-relationship facts: observed only when Envelope actually looked
            // them up (`Some(_)`); `None` means unknown → unverifiable.
            "known_contact" => self.known_contact,
            "frequent_contact" => self.frequent_contact,
            "unknown_domain" => self.unknown_domain,
            // A reply is definitively not a cold email; otherwise the store lookup.
            "cold_email" => {
                if self.is_reply {
                    Some(false)
                } else {
                    self.cold_email
                }
            }
            // Content-shape classifier facts: observed only when the classifier ran.
            "human_edited" => self.human_edited,
            "agent_drafted" => self.agent_drafted,
            "short_body" => self.short_body,
            // Host-derived keys Envelope has no signal for (e.g.
            // `unauthorized_outreach`): unobservable → declaration unverifiable.
            _ => None,
        }
    }

    /// Whether this context *definitively observed* the given host-derived key to
    /// be **false** (`conflicts_with_host_observation`). A convenience over
    /// [`Self::observe_host_key`]: only `Some(false)` is a contradiction — an
    /// unobserved (`None`) key is *unverifiable*, not *contradicted*.
    pub fn observed_false(&self, key: &str) -> bool {
        self.observe_host_key(key) == Some(false)
    }
}

/// The resolved attribution state of an actual-send attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributionState {
    /// A non-empty validated set is ready for Governor scoring.
    Attributed,
    /// No factual declaration (or nothing derivable) — `attributes_required`.
    Unattributed,
    /// One or more declarations were rejected — `attributes_invalid`.
    Invalid,
}

impl Default for AttributionState {
    fn default() -> Self {
        Self::Unattributed
    }
}

impl AttributionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attributed => "attributed",
            Self::Unattributed => "unattributed",
            Self::Invalid => "invalid",
        }
    }
}

/// A single rejected declaration, with a stable per-key reason code.
#[derive(Debug, Clone, PartialEq)]
pub struct RejectedAttr {
    pub key: String,
    /// One of: `unknown_attribute`, `attestation_required`,
    /// `conflicts_with_host_observation`, `host_verification_unavailable`,
    /// `conflicting_attributes`.
    pub code: String,
    /// Nearest catalog keys for a typo (`unknown_attribute` only).
    pub did_you_mean: Vec<String>,
    pub detail: Option<String>,
}

impl RejectedAttr {
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::json!({ "key": self.key, "code": self.code });
        if let serde_json::Value::Object(map) = &mut obj {
            if !self.did_you_mean.is_empty() {
                map.insert("did_you_mean".into(), serde_json::json!(self.did_you_mean));
            }
            if let Some(detail) = &self.detail {
                map.insert("detail".into(), serde_json::Value::String(detail.clone()));
            }
        }
        obj
    }
}

/// The three explicit attribute sets plus rejections, produced by [`resolve`].
///
/// `governor_attrs` is the validated union actually submitted to Governor — it is
/// empty unless `state == Attributed`, so an invalid or unattributed request can
/// never reach scoring.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AttributionResolution {
    pub declared_attrs: Vec<String>,
    pub derived_attrs: Vec<String>,
    pub governor_attrs: Vec<String>,
    pub rejected_attrs: Vec<RejectedAttr>,
    pub accepted_redundant: Vec<String>,
    pub state: AttributionState,
}

impl AttributionResolution {
    pub fn is_attributed(&self) -> bool {
        self.state == AttributionState::Attributed
    }

    /// Stable top-level code for a failed resolution, or `None` when attributed.
    ///
    /// The public code names the missing/invalid INPUT (`attributes`), not the
    /// internal attribution protocol: a bot recovers by supplying/correcting
    /// `attributes`, so the agent-facing code must say so.
    pub fn failure_code(&self) -> Option<&'static str> {
        match self.state {
            AttributionState::Attributed => None,
            AttributionState::Unattributed => Some("attributes_required"),
            AttributionState::Invalid => Some("attributes_invalid"),
        }
    }

    /// The additive attribution block for audit rows and responses. Contains the
    /// three sets, rejections, and state — never a score, weight, or threshold.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "protocol": crate::governor_catalog::ATTRIBUTION_PROTOCOL,
            "catalog": crate::governor_catalog::CATALOG_NAME,
            "catalog_version": catalog_version(),
            "attribution_state": self.state.as_str(),
            "declared_attrs": self.declared_attrs,
            "derived_attrs": self.derived_attrs,
            "governor_attrs": self.governor_attrs,
            "accepted_redundant": self.accepted_redundant,
            "rejected_attrs": self.rejected_attrs.iter().map(RejectedAttr::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Resolve a bot's declared attributes against Envelope's derived facts into the
/// validated set submitted to Governor.
///
/// - Unknown keys → `unknown_attribute` (with `did_you_mean`).
/// - Attestation keys (`tyler_approved`, `authorized_campaign`) → `attestation_required`
///   (never declarable, even when an attestation already exists).
/// - Host-derived keys the context observed *true* → accepted as
///   `accepted_redundant` (a corroborated declaration that counts).
/// - Host-derived keys the context observed *false* → `conflicts_with_host_observation`.
/// - Host-derived keys the context could not observe → `host_verification_unavailable`
///   (never silently accepted — an unverifiable host claim is not a declaration).
/// - Impossible combinations (exclusivity pairs) → `conflicting_attributes`.
///
/// Any rejection fails the **whole** request (`Invalid`, empty `governor_attrs`).
/// With `require_declaration` set, an empty accepted-declaration set is
/// `Unattributed` even when derived facts are non-empty — host facts never
/// substitute for the bot's attribution responsibility.
pub fn resolve(
    declared: &[String],
    ctx: &AttributedSendContext,
    require_declaration: bool,
) -> AttributionResolution {
    let derived: Vec<String> = ctx
        .to_governor_attrs()
        .into_iter()
        .map(str::to_string)
        .collect();

    // Dedupe declared, preserving first-seen order; ignore blank tokens.
    let mut declared_unique: Vec<String> = Vec::new();
    for k in declared {
        let k = k.trim();
        if !k.is_empty() && !declared_unique.iter().any(|d| d == k) {
            declared_unique.push(k.to_string());
        }
    }

    let mut accepted_declarable: Vec<String> = Vec::new();
    let mut accepted_redundant: Vec<String> = Vec::new();
    let mut rejected: Vec<RejectedAttr> = Vec::new();

    for key in &declared_unique {
        match provenance_of(key) {
            None => rejected.push(RejectedAttr {
                key: key.clone(),
                code: "unknown_attribute".into(),
                did_you_mean: nearest_keys(key, 2),
                detail: Some(format!(
                    "`{key}` is not in catalog envelope v{}",
                    catalog_version()
                )),
            }),
            Some(Provenance::RequiresAttestation) => rejected.push(RejectedAttr {
                key: key.clone(),
                code: "attestation_required".into(),
                did_you_mean: Vec::new(),
                detail: Some(
                    "recorded only by human dashboard approval or an operator signal; agent declarations are always rejected".into(),
                ),
            }),
            Some(Provenance::Declarable) => accepted_declarable.push(key.clone()),
            // Tri-state host verification: a declared host-derived key counts only
            // when Envelope independently observes it TRUE. Observed-false is a
            // contradiction; unobservable is unverifiable — neither is accepted.
            Some(Provenance::HostDerived) => match ctx.observe_host_key(key) {
                Some(true) => accepted_redundant.push(key.clone()),
                Some(false) => rejected.push(RejectedAttr {
                    key: key.clone(),
                    code: "conflicts_with_host_observation".into(),
                    did_you_mean: Vec::new(),
                    detail: Some(contradiction_detail(key)),
                }),
                None => rejected.push(RejectedAttr {
                    key: key.clone(),
                    code: "host_verification_unavailable".into(),
                    did_you_mean: Vec::new(),
                    detail: Some(unverifiable_detail(key)),
                }),
            },
        }
    }

    // Impossible combinations across the union (derived ∪ accepted declared).
    let mut union: Vec<String> = derived.clone();
    for k in accepted_declarable.iter().chain(accepted_redundant.iter()) {
        if !union.contains(k) {
            union.push(k.clone());
        }
    }
    let mut conflict_keys: Vec<String> = Vec::new();
    for key in accepted_declarable.iter().chain(accepted_redundant.iter()) {
        if let Some(partner) = conflicting_partner(key, &union) {
            conflict_keys.push(key.clone());
            rejected.push(RejectedAttr {
                key: key.clone(),
                code: "conflicting_attributes".into(),
                did_you_mean: Vec::new(),
                detail: Some(format!("`{key}` cannot be combined with `{partner}`")),
            });
        }
    }
    accepted_declarable.retain(|k| !conflict_keys.contains(k));
    accepted_redundant.retain(|k| !conflict_keys.contains(k));

    let has_declaration = !accepted_declarable.is_empty() || !accepted_redundant.is_empty();

    let (state, governor_attrs) = if !rejected.is_empty() {
        (AttributionState::Invalid, Vec::new())
    } else {
        let mut governor: Vec<String> = derived.clone();
        for k in accepted_declarable.iter().chain(accepted_redundant.iter()) {
            if !governor.contains(k) {
                governor.push(k.clone());
            }
        }
        governor.sort();
        if (require_declaration && !has_declaration) || governor.is_empty() {
            (AttributionState::Unattributed, Vec::new())
        } else {
            (AttributionState::Attributed, governor)
        }
    };

    AttributionResolution {
        declared_attrs: declared_unique,
        derived_attrs: derived,
        governor_attrs,
        rejected_attrs: rejected,
        accepted_redundant,
        state,
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    fn ctx_cold_external_attachment() -> AttributedSendContext {
        AttributedSendContext {
            account_domain: Some("martin.fm".into()),
            recipient_domains: vec!["acme.example".into()],
            recipient_count: 1,
            attachment_count: 1,
            sensitive_attachment: true,
            ..Default::default()
        }
    }

    #[test]
    fn empty_declaration_is_attributes_required_even_with_derived_facts() {
        // Non-negotiable rule 1: host-derived facts never substitute for a bot
        // declaration. A rich derived set with zero declarations is still
        // `attributes_required`.
        let ctx = ctx_cold_external_attachment();
        let res = resolve(&[], &ctx, true);
        assert_eq!(res.state, AttributionState::Unattributed);
        assert!(!res.derived_attrs.is_empty(), "context should derive facts");
        assert!(
            res.governor_attrs.is_empty(),
            "nothing may be submitted to Governor when unattributed"
        );
        assert_eq!(res.failure_code(), Some("attributes_required"));
    }

    #[test]
    fn human_originated_path_does_not_require_a_bot_declaration() {
        // When require_declaration is false (host attestation / sweep), a derived
        // set alone is submittable.
        let ctx = ctx_cold_external_attachment();
        let res = resolve(&[], &ctx, false);
        assert_eq!(res.state, AttributionState::Attributed);
        assert!(!res.governor_attrs.is_empty());
    }

    #[test]
    fn honest_declaration_reaches_governor_with_the_union() {
        let ctx = ctx_cold_external_attachment();
        let res = resolve(&["financial_content".into()], &ctx, true);
        assert_eq!(res.state, AttributionState::Attributed);
        assert!(
            res.governor_attrs
                .contains(&"financial_content".to_string())
        );
        assert!(res.governor_attrs.contains(&"has_attachment".to_string()));
        assert!(
            res.governor_attrs
                .contains(&"sensitive_attachment".to_string())
        );
        assert_eq!(res.declared_attrs, vec!["financial_content".to_string()]);
    }

    #[test]
    fn typo_is_invalid_with_did_you_mean_and_reaches_no_scoring() {
        let ctx = ctx_cold_external_attachment();
        let res = resolve(&["informationl".into()], &ctx, true);
        assert_eq!(res.state, AttributionState::Invalid);
        assert!(
            res.governor_attrs.is_empty(),
            "invalid never partially scores"
        );
        let rej = &res.rejected_attrs[0];
        assert_eq!(rej.code, "unknown_attribute");
        assert!(rej.did_you_mean.contains(&"informational".to_string()));
    }

    #[test]
    fn self_asserted_attestation_is_rejected_and_never_submitted() {
        let ctx = ctx_cold_external_attachment();
        let res = resolve(
            &["tyler_approved".into(), "financial_content".into()],
            &ctx,
            true,
        );
        assert_eq!(res.state, AttributionState::Invalid);
        assert!(
            !res.governor_attrs.iter().any(|a| a == "tyler_approved"),
            "attestation key must never appear in the submitted set"
        );
        assert_eq!(res.rejected_attrs[0].code, "attestation_required");
    }

    #[test]
    fn declaration_contradicting_host_observation_fails_the_whole_request() {
        // Declaring reply_to_thread on a non-reply.
        let ctx = ctx_cold_external_attachment();
        let res = resolve(&["reply_to_thread".into()], &ctx, true);
        assert_eq!(res.state, AttributionState::Invalid);
        assert!(res.governor_attrs.is_empty());
        assert_eq!(
            res.rejected_attrs[0].code,
            "conflicts_with_host_observation"
        );
    }

    #[test]
    fn redundant_consistent_host_declaration_is_accepted_not_dropped() {
        // Bot declares has_attachment, which the host also derived: accepted as
        // redundant, request proceeds.
        let ctx = ctx_cold_external_attachment();
        let res = resolve(&["has_attachment".into()], &ctx, true);
        assert_eq!(res.state, AttributionState::Attributed);
        assert!(
            res.accepted_redundant
                .contains(&"has_attachment".to_string())
        );
        assert!(res.governor_attrs.contains(&"has_attachment".to_string()));
    }

    #[test]
    fn impossible_combination_is_rejected() {
        // Bot declares cold_email while the host derived reply_to_thread.
        let mut ctx = ctx_cold_external_attachment();
        ctx.is_reply = true;
        let res = resolve(&["cold_email".into()], &ctx, true);
        assert_eq!(res.state, AttributionState::Invalid);
        // cold_email is rejected — either as a host contradiction (it IS a reply)
        // or an impossible combination; both fail the whole request.
        assert!(res.rejected_attrs.iter().any(|r| r.key == "cold_email"
            && (r.code == "conflicting_attributes"
                || r.code == "conflicts_with_host_observation")));
        assert!(res.governor_attrs.is_empty());
    }

    #[test]
    fn resolution_json_carries_no_score() {
        let ctx = ctx_cold_external_attachment();
        let res = resolve(&["financial_content".into()], &ctx, true);
        let text = res.to_json().to_string();
        for banned in ["\"score\"", "weight", "threshold"] {
            assert!(!text.contains(banned), "resolution json leaked {banned}");
        }
    }

    // ── Block 1: tri-state host verification ────────────────────────────────
    //
    // Operator correction: a bot MAY satisfy its declaration obligation with a
    // host-derived key **only when Envelope independently observes it true**
    // (declaration + host corroboration). Host facts alone never substitute, and
    // an unobservable/false host declaration is rejected, never silently accepted.

    #[test]
    fn consistent_host_declaration_counts_as_the_declaration() {
        // A bot declares reply_to_thread on a genuine reply. Envelope observes it
        // true, so the declaration is corroborated and satisfies the obligation.
        let ctx = AttributedSendContext {
            account_domain: Some("martin.fm".into()),
            recipient_domains: vec!["acme.example".into()],
            recipient_count: 1,
            is_reply: true,
            ..Default::default()
        };
        let res = resolve(&["reply_to_thread".into()], &ctx, true);
        assert_eq!(
            res.state,
            AttributionState::Attributed,
            "a host-verified declared key satisfies the obligation"
        );
        assert!(
            res.accepted_redundant
                .contains(&"reply_to_thread".to_string())
        );
        assert!(res.governor_attrs.contains(&"reply_to_thread".to_string()));
    }

    #[test]
    fn false_sensitive_attachment_declaration_is_rejected() {
        // sensitive_attachment declared while the host observed no sensitive
        // attachment must fail the whole request (previously silently accepted).
        let mut ctx = ctx_cold_external_attachment();
        ctx.sensitive_attachment = false;
        let res = resolve(&["sensitive_attachment".into()], &ctx, true);
        assert_eq!(res.state, AttributionState::Invalid);
        assert!(res.governor_attrs.is_empty());
        let rej = res
            .rejected_attrs
            .iter()
            .find(|r| r.key == "sensitive_attachment")
            .expect("sensitive_attachment must be rejected");
        assert_eq!(rej.code, "conflicts_with_host_observation");
    }

    #[test]
    fn unverifiable_host_declaration_is_rejected_not_accepted() {
        // A store-relationship fact Envelope did not look up (known_contact ==
        // None) cannot be corroborated: the declaration is rejected as
        // host_verification_unavailable, NOT silently counted as a declaration.
        let ctx = AttributedSendContext {
            account_domain: Some("martin.fm".into()),
            recipient_domains: vec!["acme.example".into()],
            recipient_count: 1,
            known_contact: None,
            ..Default::default()
        };
        let res = resolve(&["known_contact".into()], &ctx, true);
        assert_eq!(res.state, AttributionState::Invalid);
        assert!(res.governor_attrs.is_empty());
        let rej = res
            .rejected_attrs
            .iter()
            .find(|r| r.key == "known_contact")
            .expect("known_contact must be rejected when unverifiable");
        assert_eq!(rej.code, "host_verification_unavailable");
    }

    #[test]
    fn no_host_derived_key_is_silently_accepted_without_positive_observation() {
        // Coverage: with an empty context (no facts observed), declaring ANY
        // single host-derived key must fail the request — never Attributed. This
        // proves every HOST_DERIVED key has true/false/unknown semantics and
        // "unavailable" fails closed instead of silently satisfying the obligation.
        use crate::attribution_provenance::HOST_DERIVED;
        let ctx = AttributedSendContext::default();
        for key in HOST_DERIVED {
            let res = resolve(&[key.to_string()], &ctx, true);
            assert_ne!(
                res.state,
                AttributionState::Attributed,
                "host-derived key `{key}` was silently accepted without observation"
            );
            assert!(
                res.governor_attrs.is_empty(),
                "`{key}` reached Governor without corroboration"
            );
            let rej = res
                .rejected_attrs
                .iter()
                .find(|r| &r.key == key)
                .unwrap_or_else(|| panic!("`{key}` should have a rejection reason"));
            assert!(
                rej.code == "conflicts_with_host_observation"
                    || rej.code == "host_verification_unavailable",
                "`{key}` rejected with unexpected code {}",
                rej.code
            );
        }
    }
}

/// Human-readable detail for a host-observation contradiction.
fn contradiction_detail(key: &str) -> String {
    match key {
        "reply_to_thread" => "the message has no In-Reply-To/References; it is not a reply".into(),
        "has_attachment" => "the message has no attachments".into(),
        "has_bcc" => "the message has no BCC recipients".into(),
        "bulk_send" => "the message has fewer than 6 recipients".into(),
        "internal_domain" => "not every recipient is on the sender's domain".into(),
        "freemail_domain" => "no recipient is on a freemail domain".into(),
        "disposable_domain" => "no recipient is on a disposable-email domain".into(),
        "gov_domain" => "no recipient is on a government domain".into(),
        "cold_email" => "this message continues an existing thread; it is not a cold email".into(),
        "read_only" | "move_to_folder" | "delete_message" | "draft_only" => {
            "this is an actual send, not a read-only or draft-only action".into()
        }
        "sensitive_attachment" => "no attachment classifies as sensitive".into(),
        _ => format!("host observation contradicts `{key}`"),
    }
}

/// Human-readable detail for a host-derived key Envelope could not verify. The
/// declaration is rejected because an unverifiable host claim cannot corroborate
/// the bot's attribution obligation.
fn unverifiable_detail(key: &str) -> String {
    format!(
        "Envelope could not verify `{key}` at send time; a host-derived declaration is accepted only when Envelope independently observes it true — declare a fact Envelope can corroborate, or an author-context attribute you alone know"
    )
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
