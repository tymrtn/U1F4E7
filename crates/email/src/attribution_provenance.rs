// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Envelope-owned provenance classification for the Governor **envelope**
//! catalog keys.
//!
//! Governor owns the catalog (keys, descriptions, categories, and the private
//! weights). Envelope owns *who is allowed to assert each key*:
//!
//! - [`Provenance::Declarable`] — author-context facts the host cannot yet
//!   observe. A bot declares them; Envelope accepts them verbatim.
//! - [`Provenance::HostDerived`] — structural/store facts Envelope observes.
//!   A bot *may* redundantly declare them; Envelope accepts only when the
//!   declaration is consistent with what it observed, and rejects contradictions.
//! - [`Provenance::RequiresAttestation`] — human-authority facts. A bot can
//!   never declare these; only the host records them (revision-bound human
//!   approval, or an operator campaign signal).
//!
//! This table contains **no weights, thresholds, or scores** — only the
//! declaration policy for each key. The exhaustiveness test guarantees every
//! vendored catalog key has exactly one provenance, so a new upstream attribute
//! fails the build until it is classified here.

/// Who may assert a catalog attribute key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Author-context fact only the bot knows; accepted verbatim when declared.
    Declarable,
    /// Structural/store fact Envelope observes; a bot declaration is accepted
    /// only when consistent, and a contradiction fails the whole request.
    HostDerived,
    /// Human-authority fact; never bot-declarable, only host-recorded.
    RequiresAttestation,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declarable => "declarable",
            Self::HostDerived => "host_derived",
            Self::RequiresAttestation => "requires_attestation",
        }
    }
}

/// Author-context facts a bot declares (7). The host cannot observe these from a
/// generic CLI/MCP process. `agent_drafted` is authorship the bot alone knows —
/// Envelope has no durable, verifiable origin signal for it at send time, so it
/// is declared, never inferred (a human CLI user is never silently marked
/// agent-drafted). See [`crate::attribution::AttributedSendContext`].
pub const DECLARABLE: &[&str] = &[
    "informational",
    "scheduling",
    "low_stakes",
    "financial_content",
    "legal_content",
    "commitment_language",
    "has_pii",
    "uncited_claims",
    "agent_drafted",
];

/// Human-authority facts a bot can never declare (2).
pub const REQUIRES_ATTESTATION: &[&str] = &["tyler_approved", "authorized_campaign"];

/// Structural/store facts Envelope derives (21). A redundant, consistent bot
/// declaration is accepted (recorded as redundant); a contradiction is fatal.
pub const HOST_DERIVED: &[&str] = &[
    "reply_to_thread",
    "known_contact",
    "frequent_contact",
    "cold_email",
    "read_only",
    "draft_only",
    "move_to_folder",
    "delete_message",
    "unauthorized_outreach",
    "human_edited",
    "short_body",
    "has_attachment",
    "calendar_invitation",
    "sensitive_attachment",
    "internal_domain",
    "trusted_domain",
    "unknown_domain",
    "freemail_domain",
    "disposable_domain",
    "gov_domain",
    "bulk_send",
    "has_bcc",
    "single_recipient",
];

/// Mutually-exclusive attribute pairs. If both are present in the resolved
/// union (declared ∪ derived) the whole request is rejected as
/// `conflicting_attributes` — an impossible combination is never silently
/// dropped.
///
/// Deliberately **absent**: `human_edited` × `agent_drafted`. Per the
/// operator-reviewed design, AI origin and later human editing can both be true
/// and may intentionally offset; exclusivity there is a future catalog
/// calibration decision, not a protocol rule.
pub const EXCLUSIVITY_PAIRS: &[(&str, &str)] = &[
    ("cold_email", "known_contact"),
    ("cold_email", "frequent_contact"),
    ("cold_email", "reply_to_thread"),
    ("internal_domain", "unknown_domain"),
];

/// Classify a catalog key, or `None` if the key is not in the vendored catalog.
pub fn provenance_of(key: &str) -> Option<Provenance> {
    if DECLARABLE.contains(&key) {
        Some(Provenance::Declarable)
    } else if REQUIRES_ATTESTATION.contains(&key) {
        Some(Provenance::RequiresAttestation)
    } else if HOST_DERIVED.contains(&key) {
        Some(Provenance::HostDerived)
    } else {
        None
    }
}

/// The exclusivity partner of `key` that is also present in `present`, if any.
pub fn conflicting_partner(key: &str, present: &[String]) -> Option<&'static str> {
    for (a, b) in EXCLUSIVITY_PAIRS {
        if *a == key && present.iter().any(|p| p == b) {
            return Some(b);
        }
        if *b == key && present.iter().any(|p| p == a) {
            return Some(a);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governor_catalog;
    use std::collections::BTreeSet;

    #[test]
    fn every_catalog_key_has_exactly_one_provenance() {
        let catalog: BTreeSet<String> = governor_catalog::catalog_keys().into_iter().collect();

        let declarable: BTreeSet<String> = DECLARABLE.iter().map(|s| s.to_string()).collect();
        let attestation: BTreeSet<String> =
            REQUIRES_ATTESTATION.iter().map(|s| s.to_string()).collect();
        let derived: BTreeSet<String> = HOST_DERIVED.iter().map(|s| s.to_string()).collect();

        // No duplicates within any class.
        assert_eq!(
            declarable.len(),
            DECLARABLE.len(),
            "duplicate in DECLARABLE"
        );
        assert_eq!(
            attestation.len(),
            REQUIRES_ATTESTATION.len(),
            "duplicate in REQUIRES_ATTESTATION"
        );
        assert_eq!(
            derived.len(),
            HOST_DERIVED.len(),
            "duplicate in HOST_DERIVED"
        );

        // Pairwise disjoint.
        assert!(declarable.is_disjoint(&attestation));
        assert!(declarable.is_disjoint(&derived));
        assert!(attestation.is_disjoint(&derived));

        // Union == catalog: every provenance key is a real catalog key, and every
        // catalog key is classified exactly once.
        let mut union: BTreeSet<String> = BTreeSet::new();
        union.extend(declarable.iter().cloned());
        union.extend(attestation.iter().cloned());
        union.extend(derived.iter().cloned());
        assert_eq!(
            union, catalog,
            "provenance classification must cover exactly the vendored catalog keys"
        );

        // Every key resolves to exactly one provenance via the accessor too.
        for key in &catalog {
            assert!(
                provenance_of(key).is_some(),
                "catalog key {key} has no provenance"
            );
        }
        assert!(provenance_of("totally_made_up_key").is_none());
    }

    #[test]
    fn exclusivity_partners_are_catalog_keys() {
        let catalog: BTreeSet<String> = governor_catalog::catalog_keys().into_iter().collect();
        for (a, b) in EXCLUSIVITY_PAIRS {
            assert!(catalog.contains(*a), "{a} not a catalog key");
            assert!(catalog.contains(*b), "{b} not a catalog key");
        }
    }

    #[test]
    fn conflicting_partner_detects_impossible_combinations() {
        let present = vec!["known_contact".to_string(), "short_body".to_string()];
        assert_eq!(
            conflicting_partner("cold_email", &present),
            Some("known_contact")
        );
        assert_eq!(conflicting_partner("short_body", &present), None);
        // Order independence.
        let present = vec!["cold_email".to_string()];
        assert_eq!(
            conflicting_partner("reply_to_thread", &present),
            Some("cold_email")
        );
    }

    #[test]
    fn provenance_str_names_are_stable() {
        assert_eq!(Provenance::Declarable.as_str(), "declarable");
        assert_eq!(Provenance::HostDerived.as_str(), "host_derived");
        assert_eq!(
            Provenance::RequiresAttestation.as_str(),
            "requires_attestation"
        );
    }
}
