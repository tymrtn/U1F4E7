// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Sender identity analyzer: display-name spoofing, Reply-To diversion,
//! punycode and look-alike sender domains (against the mailbox's own domain
//! and the domains it already corresponds with).

use super::domains::{domain_of, has_punycode, lookalike_of, registrable, to_unicode};
use super::{Signal, ThreatInput};

pub const LOOKALIKE_DOMAIN: u32 = 45;
pub const DISPLAY_NAME_SPOOF: u32 = 25;
pub const PUNYCODE_SENDER: u32 = 15;
pub const REPLY_TO_MISMATCH: u32 = 10;

pub fn analyze(input: &ThreatInput) -> Vec<Signal> {
    let mut signals = Vec::new();
    let Some(from_domain) = input.from_domain() else {
        return signals;
    };
    let from_reg = registrable(&from_domain);

    if let Some(display) = &input.from_display
        && let Some(claimed) = display
            .split(|c: char| {
                c.is_whitespace() || c == '<' || c == '>' || c == '"' || c == '(' || c == ')'
            })
            .find(|w| w.contains('@'))
            .and_then(domain_of)
        && registrable(&claimed) != from_reg
    {
        signals.push(Signal::new(
            "display_name_spoof",
            DISPLAY_NAME_SPOOF,
            format!("display name shows {claimed}, sent from {from_domain}"),
        ));
    }

    if has_punycode(&from_domain) {
        signals.push(Signal::new(
            "punycode_sender",
            PUNYCODE_SENDER,
            format!("{from_domain} renders as {}", to_unicode(&from_domain)),
        ));
    }

    let known = input.known_domains();
    if let Some(target) = lookalike_of(&from_domain, &known) {
        signals.push(Signal::new(
            "lookalike_domain",
            LOOKALIKE_DOMAIN,
            format!("sender domain {from_domain} imitates {target}"),
        ));
    }

    if let Some(diverted) = input
        .reply_to
        .iter()
        .filter_map(|r| domain_of(r))
        .find(|d| registrable(d) != from_reg)
    {
        signals.push(Signal::new(
            "reply_to_mismatch",
            REPLY_TO_MISMATCH,
            format!("replies go to {diverted}, sent from {from_domain}"),
        ));
    }
    signals
}

#[cfg(test)]
mod tests {
    use super::super::CorrespondentFacts;
    use super::super::test_support::{codes, input_from};
    use super::*;

    #[test]
    fn lookalike_of_a_known_correspondent_is_flagged() {
        let mut input = input_from(&["From: Billing <billing@acme-supp1y.com>"], "x");
        input.ledger = Ok(CorrespondentFacts {
            known_domains: vec!["acme-supply.com".to_string()],
            ..CorrespondentFacts::default()
        });
        let signals = analyze(&input);
        assert_eq!(codes(&signals), vec!["lookalike_domain"]);
        assert!(signals[0].evidence.contains("acme-supply.com"));
    }

    #[test]
    fn lookalike_of_the_mailbox_own_domain_is_flagged_without_ledger_rows() {
        // Account is me@example.org (see test_support).
        let input = input_from(&["From: IT Desk <it@examp1e.org>"], "x");
        assert_eq!(codes(&analyze(&input)), vec!["lookalike_domain"]);
    }

    #[test]
    fn punycode_sender_is_flagged_and_also_matches_as_lookalike() {
        let spoof = idna::domain_to_ascii("p\u{0430}ypal.com").unwrap();
        let mut input = input_from(&[&format!("From: PayPal <service@{spoof}>")], "x");
        input.ledger = Ok(CorrespondentFacts {
            known_domains: vec!["paypal.com".to_string()],
            ..CorrespondentFacts::default()
        });
        assert_eq!(
            codes(&analyze(&input)),
            vec!["punycode_sender", "lookalike_domain"]
        );
    }

    #[test]
    fn display_name_address_spoof_and_reply_to_diversion() {
        let input = input_from(
            &[
                "From: \"ceo@bigcorp.example\" <random@freemail.example>",
                "Reply-To: wire-desk@elsewhere.example",
            ],
            "x",
        );
        assert_eq!(
            codes(&analyze(&input)),
            vec!["display_name_spoof", "reply_to_mismatch"]
        );
    }

    #[test]
    fn ordinary_sender_is_clean() {
        let input = input_from(
            &[
                "From: Alice <alice@partner.example>",
                "Reply-To: alice@mail.partner.example",
            ],
            "x",
        );
        assert!(analyze(&input).is_empty());
    }
}
