// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Deterministic, weight-free contextual attribution suggestions.
//!
//! This engine lives where Governor's weights do **not**, so it structurally
//! cannot rank by score impact even if asked to. It restates catalog facts as
//! declaration conditions ("declare if the body discusses money") — it never
//! tells a bot how to improve, boost, or get allowed. Suggestions are:
//!
//! - **Factual & contextual** — derived from the action's own observed facts.
//! - **Deterministic** — a fixed rule table, no randomness, no model calls.
//! - **Bounded** — between 3 and 6 entries.
//! - **Risk-first** — risk declarables always sort before favorable ones, and at
//!   least one risk key appears whenever any risk trigger matched.
//!
//! The wording lint (tested) forbids score/weight/threshold/comparative language.

use serde_json::{Value, json};

use crate::attribution::{AttributedSendContext, RejectedAttr};

/// The five risk-side declarables. Their presence in a suggestion set is the
/// risk-inclusion invariant.
pub const RISK_KEYS: &[&str] = &[
    "financial_content",
    "legal_content",
    "commitment_language",
    "has_pii",
    "uncited_claims",
];

/// Whether `key` is a risk-side declarable.
pub fn is_risk_key(key: &str) -> bool {
    RISK_KEYS.contains(&key)
}

/// One suggested attribute: a factual declaration condition, or a labelled
/// non-declarable path (attestation) or repair (typo fix).
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub key: String,
    /// `declarable`, `requires_attestation`, or `repair`.
    pub provenance: String,
    pub declare_if: Option<String>,
    pub note: Option<String>,
}

impl Suggestion {
    pub fn to_json(&self) -> Value {
        let mut obj = json!({ "key": self.key, "provenance": self.provenance });
        if let Value::Object(map) = &mut obj {
            if let Some(d) = &self.declare_if {
                map.insert("declare_if".into(), Value::String(d.clone()));
            }
            if let Some(n) = &self.note {
                map.insert("note".into(), Value::String(n.clone()));
            }
        }
        obj
    }
}

fn declarable(key: &str) -> Suggestion {
    Suggestion {
        key: key.to_string(),
        provenance: "declarable".to_string(),
        declare_if: Some(declare_if(key).to_string()),
        note: None,
    }
}

/// Restates the catalog description as a factual declaration condition.
fn declare_if(key: &str) -> &'static str {
    match key {
        "financial_content" => "the body or an attachment discusses money, invoices, or payments",
        "legal_content" => "the message has legal implications (contracts, IP, or DMCA)",
        "commitment_language" => "the body makes promises or commitments on the human's behalf",
        "has_pii" => "the body contains personal data about any person",
        "uncited_claims" => "the body states factual claims without citing sources",
        "informational" => "the body is purely a status update or FYI",
        _ => "the fact is true of this message",
    }
}

/// Whether any recipient is external to the sender's domain (or the sender
/// domain is unknown and there are recipients).
fn has_external_recipient(ctx: &AttributedSendContext) -> bool {
    if ctx.recipient_domains.is_empty() {
        return false;
    }
    match &ctx.account_domain {
        Some(acct) => ctx.recipient_domains.iter().any(|d| d != acct),
        None => true,
    }
}

/// Generate bounded, risk-first, deterministic suggestions for this action.
///
/// `route_is_review` is true when Governor routed this send to review (post-scan
/// recovery); `has_attestation` is true when a valid human approval already
/// exists (so the attestation path is not re-suggested).
pub fn suggest(
    ctx: &AttributedSendContext,
    rejected: &[RejectedAttr],
    route_is_review: bool,
    has_attestation: bool,
) -> Vec<Suggestion> {
    let mut risk: Vec<Suggestion> = Vec::new();

    // R1 / R2 — attachment-conditioned risk declarables.
    if ctx.attachment_count > 0 {
        risk.push(declarable("financial_content"));
        risk.push(declarable("legal_content"));
    } else {
        risk.push(declarable("financial_content"));
        risk.push(declarable("commitment_language"));
    }
    // R3 (pii) — external recipients raise PII stakes.
    if has_external_recipient(ctx) {
        risk.push(declarable("has_pii"));
    }
    // R6 — a body is always present on a send; uncited claims are declarable.
    risk.push(declarable("uncited_claims"));

    // Dedupe, cap risk at 3 so favorable + attestation + repair fit within 6.
    dedupe(&mut risk);
    risk.truncate(3);

    let mut out: Vec<Suggestion> = risk;

    // R4 — one typo repair for an unknown declared key.
    if let Some(repair) = rejected.iter().find_map(|r| {
        (r.code == "unknown_attribute")
            .then(|| r.did_you_mean.first())
            .flatten()
            .map(|k| Suggestion {
                key: k.clone(),
                provenance: "repair".to_string(),
                declare_if: None,
                note: Some(format!("did you mean `{k}`?")),
            })
    }) {
        out.push(repair);
    }

    // R3 (informational) — the one favorable-context declarable.
    out.push(declarable("informational"));

    // R5 — the attestation path, labelled as a path (never declarable), at most
    // one, and only when a human authority is actually the missing signal.
    if !has_attestation && (route_is_review || out.len() < 6) {
        out.push(Suggestion {
            key: "tyler_approved".to_string(),
            provenance: "requires_attestation".to_string(),
            declare_if: None,
            note: Some(
                "cannot be declared; recorded only when a human approves the draft in the dashboard"
                    .to_string(),
            ),
        });
    }

    dedupe(&mut out);
    out.truncate(6);
    out
}

fn dedupe(list: &mut Vec<Suggestion>) {
    let mut seen: Vec<String> = Vec::new();
    list.retain(|s| {
        if seen.contains(&s.key) {
            false
        } else {
            seen.push(s.key.clone());
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_attachment_external() -> AttributedSendContext {
        AttributedSendContext {
            account_domain: Some("martin.fm".into()),
            recipient_domains: vec!["acme.example".into()],
            recipient_count: 1,
            attachment_count: 1,
            ..Default::default()
        }
    }

    #[test]
    fn suggestions_are_bounded_and_risk_first() {
        let s = suggest(&ctx_with_attachment_external(), &[], false, false);
        assert!(
            s.len() >= 3 && s.len() <= 6,
            "bounded 3..=6, got {}",
            s.len()
        );
        // At least one risk key, and it appears before any favorable declarable.
        let first_risk = s.iter().position(|x| is_risk_key(&x.key));
        let first_fav = s.iter().position(|x| x.key == "informational");
        assert!(
            first_risk.is_some(),
            "a risk trigger matched -> a risk key must appear"
        );
        if let (Some(r), Some(f)) = (first_risk, first_fav) {
            assert!(r < f, "risk declarables must sort before favorable ones");
        }
    }

    #[test]
    fn at_most_one_attestation_entry_and_it_is_not_declarable() {
        let s = suggest(&ctx_with_attachment_external(), &[], true, false);
        let att: Vec<_> = s
            .iter()
            .filter(|x| x.provenance == "requires_attestation")
            .collect();
        assert!(att.len() <= 1);
        if let Some(a) = att.first() {
            assert!(
                a.declare_if.is_none(),
                "attestation is a path, never declarable"
            );
        }
    }

    #[test]
    fn attestation_not_suggested_when_already_attested() {
        let s = suggest(&ctx_with_attachment_external(), &[], true, true);
        assert!(!s.iter().any(|x| x.provenance == "requires_attestation"));
    }

    #[test]
    fn typo_produces_a_repair_suggestion() {
        let rejected = vec![RejectedAttr {
            key: "informationl".into(),
            code: "unknown_attribute".into(),
            did_you_mean: vec!["informational".into()],
            detail: None,
        }];
        let s = suggest(&ctx_with_attachment_external(), &rejected, false, false);
        assert!(
            s.iter()
                .any(|x| x.provenance == "repair" && x.key == "informational")
        );
    }

    #[test]
    fn wording_never_teaches_how_to_get_allowed() {
        // Exhaustively lint every string across representative contexts.
        let ctxs = [
            ctx_with_attachment_external(),
            AttributedSendContext {
                recipient_domains: vec!["acme.example".into()],
                recipient_count: 1,
                ..Default::default()
            },
        ];
        let banned = [
            "increase",
            "improve",
            "boost",
            "score",
            "weight",
            "threshold",
            "to get allowed",
            "higher",
            "lower",
            "better",
            "more likely",
        ];
        for ctx in &ctxs {
            for review in [true, false] {
                for attested in [true, false] {
                    for s in suggest(ctx, &[], review, attested) {
                        let text = format!(
                            "{} {} {}",
                            s.declare_if.unwrap_or_default(),
                            s.note.unwrap_or_default(),
                            s.provenance
                        )
                        .to_lowercase();
                        for b in banned {
                            assert!(!text.contains(b), "suggestion wording leaked `{b}`: {text}");
                        }
                    }
                }
            }
        }
    }
}
