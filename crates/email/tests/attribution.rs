// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Unit tests for the shared Envelope attribution primitive.
//!
//! These tests are pure: they never open a socket, touch a mailbox, or shell
//! out to Governor. They assert that observable runtime/mailbox facts map to the
//! canonical Governor **envelope** catalog attribute keys — and that unknown
//! signals are *omitted*, never fabricated as `false`, and that no message body,
//! full recipient address, or secret ever appears in the emitted key set.

use envelope_email_transport::attribution::{
    AttributedSendContext, classify_sensitive_attachment, collect_recipient_domains,
    is_disposable_domain, is_freemail_domain, is_gov_domain,
};

/// Every key the send-side attribution primitive is allowed to emit. Emitting
/// anything outside this set means Envelope invented a key Governor's envelope
/// catalog does not score.
const CANONICAL_SEND_KEYS: &[&str] = &[
    "reply_to_thread",
    "known_contact",
    "frequent_contact",
    "cold_email",
    "tyler_approved",
    "draft_only",
    "informational",
    "human_edited",
    "short_body",
    "agent_drafted",
    "has_attachment",
    "has_pii",
    "uncited_claims",
    "sensitive_attachment",
    "internal_domain",
    "trusted_domain",
    "unknown_domain",
    "freemail_domain",
    "disposable_domain",
    "gov_domain",
    "financial_content",
    "legal_content",
    "commitment_language",
    "bulk_send",
    "has_bcc",
];

#[test]
fn threaded_known_internal_contact_emits_trust_keys() {
    let ctx = AttributedSendContext {
        account_domain: Some("martin.fm".into()),
        recipient_domains: vec!["martin.fm".into()],
        recipient_count: 1,
        is_reply: true,
        known_contact: Some(true),
        frequent_contact: Some(true),
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    assert!(attrs.contains(&"reply_to_thread"));
    assert!(attrs.contains(&"known_contact"));
    assert!(attrs.contains(&"frequent_contact"));
    assert!(attrs.contains(&"internal_domain"));
    // A trusted internal reply must not carry cold/unknown risk noise.
    assert!(!attrs.contains(&"unknown_domain"));
    assert!(!attrs.contains(&"cold_email"));
}

#[test]
fn external_unknown_freemail_recipient_emits_risk_keys() {
    let ctx = AttributedSendContext {
        account_domain: Some("martin.fm".into()),
        recipient_domains: vec!["gmail.com".into()],
        recipient_count: 1,
        agent_drafted: Some(true),
        cold_email: Some(true),
        unknown_domain: Some(true),
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    assert!(attrs.contains(&"freemail_domain"));
    assert!(attrs.contains(&"unknown_domain"));
    assert!(attrs.contains(&"cold_email"));
    assert!(attrs.contains(&"agent_drafted"));
    assert!(!attrs.contains(&"internal_domain"));
    assert!(!attrs.contains(&"known_contact"));
}

#[test]
fn sensitive_attachment_emits_attachment_keys() {
    let ctx = AttributedSendContext {
        account_domain: Some("martin.fm".into()),
        recipient_domains: vec!["example.com".into()],
        recipient_count: 1,
        attachment_count: 1,
        sensitive_attachment: true,
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    assert!(attrs.contains(&"has_attachment"));
    assert!(attrs.contains(&"sensitive_attachment"));
}

#[test]
fn attachment_present_but_not_sensitive_only_emits_has_attachment() {
    let ctx = AttributedSendContext {
        attachment_count: 2,
        sensitive_attachment: false,
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    assert!(attrs.contains(&"has_attachment"));
    assert!(!attrs.contains(&"sensitive_attachment"));
}

#[test]
fn draft_only_human_approved_edited_emit_intent_keys() {
    let ctx = AttributedSendContext {
        draft_only: true,
        human_approved: true,
        human_edited: Some(true),
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    assert!(attrs.contains(&"draft_only"));
    assert!(attrs.contains(&"tyler_approved"));
    assert!(attrs.contains(&"human_edited"));
}

#[test]
fn human_edited_and_agent_drafted_can_both_be_true() {
    // Operator-reviewed correction: AI origin and later human editing can BOTH be
    // true and may intentionally offset one another. The protocol does not force
    // them mutually exclusive until catalog semantics are calibrated; each is
    // emitted on its own observed truth.
    let ctx = AttributedSendContext {
        human_edited: Some(true),
        agent_drafted: Some(true),
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    assert!(attrs.contains(&"human_edited"));
    assert!(attrs.contains(&"agent_drafted"));
}

#[test]
fn agent_drafted_emitted_when_not_human_edited() {
    let ctx = AttributedSendContext {
        agent_drafted: Some(true),
        human_edited: Some(false),
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    assert!(attrs.contains(&"agent_drafted"));
    assert!(!attrs.contains(&"human_edited"));
}

#[test]
fn unknown_classifiers_are_omitted_not_fabricated() {
    // Classifiers that don't exist yet are left None → the attribute is omitted
    // entirely. Omission is honest; a fabricated `false` would understate risk.
    let ctx = AttributedSendContext {
        account_domain: Some("martin.fm".into()),
        recipient_domains: vec!["example.com".into()],
        recipient_count: 1,
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    for omitted in [
        "has_pii",
        "financial_content",
        "legal_content",
        "commitment_language",
        "uncited_claims",
        "informational",
        "short_body",
        "known_contact",
        "frequent_contact",
        "cold_email",
        "unknown_domain",
        "agent_drafted",
        "human_edited",
    ] {
        assert!(
            !attrs.contains(&omitted),
            "{omitted} must be omitted when not derived, got {attrs:?}"
        );
    }
}

#[test]
fn content_classifiers_emit_when_observed_true() {
    let ctx = AttributedSendContext {
        has_pii: Some(true),
        financial_content: Some(true),
        legal_content: Some(true),
        commitment_language: Some(true),
        uncited_claims: Some(true),
        informational: Some(true),
        short_body: Some(true),
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    for key in [
        "has_pii",
        "financial_content",
        "legal_content",
        "commitment_language",
        "uncited_claims",
        "informational",
        "short_body",
    ] {
        assert!(attrs.contains(&key), "{key} should emit when Some(true)");
    }
}

#[test]
fn content_classifiers_omitted_when_observed_false() {
    let ctx = AttributedSendContext {
        has_pii: Some(false),
        financial_content: Some(false),
        short_body: Some(false),
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    assert!(!attrs.contains(&"has_pii"));
    assert!(!attrs.contains(&"financial_content"));
    assert!(!attrs.contains(&"short_body"));
}

#[test]
fn bulk_send_and_bcc_emit_recipient_keys() {
    let ctx = AttributedSendContext {
        recipient_domains: vec!["example.com".into()],
        recipient_count: 7,
        has_bcc: true,
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    assert!(attrs.contains(&"bulk_send"));
    assert!(attrs.contains(&"has_bcc"));
}

#[test]
fn six_recipients_is_bulk_five_is_not() {
    let five = AttributedSendContext {
        recipient_count: 5,
        ..Default::default()
    };
    assert!(!five.to_governor_attrs().contains(&"bulk_send"));
    let six = AttributedSendContext {
        recipient_count: 6,
        ..Default::default()
    };
    assert!(six.to_governor_attrs().contains(&"bulk_send"));
}

#[test]
fn trusted_domain_emitted_when_recipient_on_allowlist() {
    let ctx = AttributedSendContext {
        account_domain: Some("martin.fm".into()),
        recipient_domains: vec!["partner.com".into()],
        recipient_count: 1,
        trusted_domains: vec!["partner.com".into()],
        ..Default::default()
    };
    assert!(ctx.to_governor_attrs().contains(&"trusted_domain"));
}

#[test]
fn internal_domain_requires_all_recipients_internal() {
    let mixed = AttributedSendContext {
        account_domain: Some("martin.fm".into()),
        recipient_domains: vec!["martin.fm".into(), "gmail.com".into()],
        recipient_count: 2,
        ..Default::default()
    };
    assert!(!mixed.to_governor_attrs().contains(&"internal_domain"));

    let all_internal = AttributedSendContext {
        account_domain: Some("martin.fm".into()),
        recipient_domains: vec!["martin.fm".into()],
        recipient_count: 1,
        ..Default::default()
    };
    assert!(
        all_internal
            .to_governor_attrs()
            .contains(&"internal_domain")
    );
}

#[test]
fn output_is_only_canonical_keys_and_never_leaks_pii() {
    // A rich context whose raw inputs contain a sensitive client domain. The
    // emitted key set must be canonical catalog keys only — never the domain,
    // never an address, never a body fragment.
    let ctx = AttributedSendContext {
        account_domain: Some("martin.fm".into()),
        recipient_domains: vec!["secret-client-acquisition.com".into(), "gmail.com".into()],
        recipient_count: 2,
        is_reply: true,
        attachment_count: 1,
        sensitive_attachment: true,
        has_bcc: true,
        human_approved: true,
        financial_content: Some(true),
        ..Default::default()
    };
    let attrs = ctx.to_governor_attrs();
    for a in &attrs {
        assert!(
            CANONICAL_SEND_KEYS.contains(a),
            "{a} is not a canonical catalog key"
        );
        assert!(!a.contains('@'), "{a} looks like an address");
        assert!(
            !a.contains("secret-client"),
            "{a} leaked a recipient domain"
        );
        assert!(!a.contains("martin.fm"), "{a} leaked the sender domain");
    }
}

#[test]
fn empty_context_emits_nothing() {
    // No observable facts → no attributes. Governor will score this at the base
    // (review), which is the honest "we know nothing" outcome.
    let attrs = AttributedSendContext::default().to_governor_attrs();
    assert!(attrs.is_empty(), "expected no attrs, got {attrs:?}");
}

// ── Classification helpers (observable, not scoring) ─────────────────────────

#[test]
fn freemail_classification() {
    assert!(is_freemail_domain("gmail.com"));
    assert!(is_freemail_domain("GMAIL.COM"));
    assert!(is_freemail_domain("yahoo.com"));
    assert!(is_freemail_domain("outlook.com"));
    assert!(is_freemail_domain("icloud.com"));
    assert!(!is_freemail_domain("martin.fm"));
    assert!(!is_freemail_domain("aposema.com"));
}

#[test]
fn disposable_classification() {
    assert!(is_disposable_domain("mailinator.com"));
    assert!(is_disposable_domain("guerrillamail.com"));
    assert!(!is_disposable_domain("gmail.com"));
    assert!(!is_disposable_domain("martin.fm"));
}

#[test]
fn gov_classification() {
    assert!(is_gov_domain("whitehouse.gov"));
    assert!(is_gov_domain("army.mil"));
    assert!(is_gov_domain("sede.gob.es"));
    assert!(!is_gov_domain("example.com"));
    assert!(!is_gov_domain("governor.io"));
}

#[test]
fn collect_recipient_domains_dedups_and_counts() {
    let summary = collect_recipient_domains(
        "Alice <a@example.com>, b@example.com",
        Some("c@sub.example.com"),
        Some("d@other.com"),
    );
    // 4 addresses across to/cc/bcc.
    assert_eq!(summary.count, 4);
    assert!(summary.domains.contains(&"example.com".to_string()));
    assert!(summary.domains.contains(&"sub.example.com".to_string()));
    assert!(summary.domains.contains(&"other.com".to_string()));
    assert!(summary.has_bcc);
    // No full addresses retained.
    assert!(!summary.domains.iter().any(|d| d.contains('@')));
}

#[test]
fn collect_recipient_domains_without_bcc() {
    let summary = collect_recipient_domains("a@example.com", None, None);
    assert_eq!(summary.count, 1);
    assert!(!summary.has_bcc);
}

#[test]
fn sensitive_attachment_classifier_flags_contracts_and_finance() {
    assert!(classify_sensitive_attachment(
        "Q1-invoice.pdf",
        "application/pdf"
    ));
    assert!(classify_sensitive_attachment(
        "NDA_final.docx",
        "application/octet-stream"
    ));
    assert!(classify_sensitive_attachment(
        "Master Services Agreement.pdf",
        "application/pdf"
    ));
    assert!(classify_sensitive_attachment(
        "2025-tax-return.pdf",
        "application/pdf"
    ));
    assert!(!classify_sensitive_attachment(
        "cat-photo.jpg",
        "image/jpeg"
    ));
    assert!(!classify_sensitive_attachment("notes.txt", "text/plain"));
}
