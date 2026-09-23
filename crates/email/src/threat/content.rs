// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Content analyzer: the lures phishing copy leans on (credential
//! verification, payment redirection, manufactured urgency). Weights are
//! deliberately small; wording alone never makes a message suspicious, it
//! tips a message that already has a structural signal.
//!
//! Evidence names the lure category and how many phrases matched, never the
//! phrases themselves as found in the body.

use super::{Signal, ThreatInput};

pub const CREDENTIAL_LURE: u32 = 12;
pub const PAYMENT_LURE: u32 = 10;
pub const URGENCY: u32 = 5;

const CREDENTIAL_PHRASES: &[&str] = &[
    "verify your account",
    "verify your identity",
    "confirm your password",
    "confirm your account",
    "re-enter your password",
    "update your payment",
    "update your billing",
    "account has been suspended",
    "account has been locked",
    "unusual sign-in activity",
    "unusual login activity",
    "login to restore",
    "log in to restore",
    "validate your mailbox",
    "your password expires",
    "mailbox is full",
];

const PAYMENT_PHRASES: &[&str] = &[
    "wire transfer",
    "gift card",
    "gift cards",
    "bitcoin",
    "change of bank details",
    "new bank account",
    "updated banking details",
    "outstanding invoice",
    "overdue payment",
];

const URGENCY_PHRASES: &[&str] = &[
    "within 24 hours",
    "within 48 hours",
    "immediately",
    "urgent",
    "final notice",
    "act now",
    "will be closed",
    "will be deleted",
    "failure to comply",
];

fn count(haystack: &str, phrases: &[&str]) -> usize {
    phrases.iter().filter(|p| haystack.contains(*p)).count()
}

pub fn analyze(input: &ThreatInput) -> Vec<Signal> {
    let mut haystack = String::new();
    for (name, value) in &input.headers {
        if name.eq_ignore_ascii_case("subject") {
            haystack.push_str(value);
            haystack.push('\n');
        }
    }
    if let Some(text) = &input.text {
        haystack.push_str(text);
    }
    let haystack = haystack.to_lowercase();

    let mut signals = Vec::new();
    for (code, weight, phrases) in [
        ("credential_lure", CREDENTIAL_LURE, CREDENTIAL_PHRASES),
        ("payment_lure", PAYMENT_LURE, PAYMENT_PHRASES),
        ("urgency", URGENCY, URGENCY_PHRASES),
    ] {
        let hits = count(&haystack, phrases);
        if hits > 0 {
            signals.push(Signal::new(
                code,
                weight,
                format!("{hits} {} phrase(s)", code.replace('_', " ")),
            ));
        }
    }
    signals
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{codes, input_from};
    use super::*;

    #[test]
    fn lure_categories_are_counted_without_quoting_the_body() {
        let input = input_from(
            &[
                "From: a@b.example",
                "Subject: URGENT: account has been suspended",
            ],
            "Please verify your account within 24 hours or it will be closed.",
        );
        let signals = analyze(&input);
        assert_eq!(codes(&signals), vec!["credential_lure", "urgency"]);
        assert_eq!(signals[0].evidence, "2 credential lure phrase(s)");
        assert!(!signals.iter().any(|s| s.evidence.contains("verify")));
    }

    #[test]
    fn ordinary_mail_has_no_content_signals() {
        let input = input_from(
            &["From: a@b.example", "Subject: Lunch?"],
            "Want to grab lunch on Thursday?",
        );
        assert!(analyze(&input).is_empty());
    }

    #[test]
    fn wording_alone_stays_below_suspicious() {
        let input = input_from(
            &["From: a@b.example", "Subject: Final notice"],
            "Urgent: verify your account and send a gift card immediately.",
        );
        let total: u32 = analyze(&input).iter().map(|s| s.weight).sum();
        assert!(total < super::super::SUSPICIOUS_THRESHOLD, "total {total}");
    }
}
