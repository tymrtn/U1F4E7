// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Correspondent-ledger analyzer: a first message from a stranger, and a
//! stranger borrowing the display name of someone this mailbox knows.
//!
//! Public v1 reads [`CorrespondentFacts`] from `contacts` and cached
//! `thread_messages` (see `Database::correspondent_facts`).
//!
//! v2 enrichment point: private v2 keeps an address-keyed relationship
//! ledger (receipts, folded relationship facts, FRV warmth). When this merges
//! forward, `persist::load_ledger` should fill `CorrespondentFacts` from that
//! ledger instead — replies exchanged, warmth, first-contact date — and this
//! analyzer can weigh a cold sender against a warm one without changing its
//! signal codes.

use super::{Analyzer, CorrespondentFacts, Signal, ThreatInput};

pub const KNOWN_NAME_NEW_ADDRESS: u32 = 30;
pub const FIRST_CONTACT: u32 = 5;

pub fn analyze_facts(input: &ThreatInput, facts: &CorrespondentFacts) -> Vec<Signal> {
    let mut signals = Vec::new();
    let stranger = !facts.known_contact && facts.prior_inbound == 0 && facts.prior_outbound == 0;

    if !facts.name_matches.is_empty() && !facts.name_matches.contains(&input.from_addr) {
        let known_domains: Vec<String> = facts
            .name_matches
            .iter()
            .filter_map(|a| super::domains::domain_of(a))
            .collect();
        signals.push(Signal::new(
            "known_name_new_address",
            KNOWN_NAME_NEW_ADDRESS,
            format!(
                "display name belongs to a contact at {}; sent from {}",
                known_domains.join(", "),
                input.from_domain().unwrap_or_default()
            ),
        ));
    }
    if stranger {
        signals.push(Signal::new(
            "first_contact",
            FIRST_CONTACT,
            format!(
                "no prior mail with this sender at {}",
                input.from_domain().unwrap_or_default()
            ),
        ));
    }
    signals
}

/// Fails (and so fails the verdict closed) when the ledger could not be read.
pub struct LedgerAnalyzer;

impl Analyzer for LedgerAnalyzer {
    fn name(&self) -> &'static str {
        "ledger"
    }

    fn analyze(&self, input: &ThreatInput) -> Result<Vec<Signal>, String> {
        match &input.ledger {
            Ok(facts) => Ok(analyze_facts(input, facts)),
            Err(reason) => Err(reason.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{codes, input_from};
    use super::*;

    #[test]
    fn stranger_borrowing_a_known_name() {
        let input = input_from(&["From: Dana Chief <dana.chief@freemail.example>"], "x");
        let facts = CorrespondentFacts {
            name_matches: vec!["ceo@acme.example".to_string()],
            ..CorrespondentFacts::default()
        };
        let signals = analyze_facts(&input, &facts);
        assert_eq!(
            codes(&signals),
            vec!["known_name_new_address", "first_contact"]
        );
        assert!(signals[0].evidence.contains("acme.example"));
        assert!(!signals[0].evidence.contains("ceo@"));
    }

    #[test]
    fn known_correspondent_is_quiet() {
        let input = input_from(&["From: Dana Chief <ceo@acme.example>"], "x");
        let facts = CorrespondentFacts {
            known_contact: true,
            prior_inbound: 4,
            prior_outbound: 2,
            name_matches: vec!["ceo@acme.example".to_string()],
            ..CorrespondentFacts::default()
        };
        assert!(analyze_facts(&input, &facts).is_empty());
    }
}
