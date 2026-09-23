// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Authentication-Results (RFC 8601) analyzer.
//!
//! Only an A-R header written by the receiving host counts. RFC 8601 §5 says
//! the receiving MTA should strip pre-existing headers carrying its own
//! authserv-id, but many do not, so header *order* is the defence: the
//! receiving host prepends its own `Received` lines, and a genuine A-R sits
//! above the lowest of them. We trust only the topmost A-R whose authserv-id
//! belongs to the receiving domain and that appears above that boundary. Any
//! other A-R claiming the receiving domain (lower in the block, or a second
//! one) was supplied by someone else: `ar_forged`. A-R headers from other
//! authserv-ids are ignored entirely.

use super::domains::registrable;
use super::{Signal, ThreatInput, is_dns_name, received_by_host};

pub const AR_FORGED: u32 = 40;
pub const DMARC_FAIL: u32 = 35;
pub const SPF_FAIL: u32 = 15;
pub const SPF_SOFTFAIL: u32 = 8;
pub const DKIM_FAIL: u32 = 12;
pub const AUTH_UNVERIFIABLE: u32 = 5;

pub fn analyze(input: &ThreatInput) -> Vec<Signal> {
    let Some(receiving) = input.receiving_host.as_deref() else {
        return vec![Signal::new(
            "auth_unverifiable",
            AUTH_UNVERIFIABLE,
            "no Received header names a receiving host",
        )];
    };
    let receiving_domain = registrable(receiving);

    // Boundary: the lowest Received header in the unbroken top run added by
    // the receiving domain (opaque internal hops like `by 2002:a05:...`
    // belong to the run).
    let mut boundary: Option<usize> = None;
    for (idx, (name, value)) in input.headers.iter().enumerate() {
        if !name.eq_ignore_ascii_case("received") {
            continue;
        }
        let internal = match received_by_host(value) {
            Some(host) if is_dns_name(&host) => registrable(&host) == receiving_domain,
            _ => true,
        };
        if internal {
            boundary = Some(idx);
        } else {
            break;
        }
    }
    let Some(boundary) = boundary else {
        return vec![Signal::new(
            "auth_unverifiable",
            AUTH_UNVERIFIABLE,
            "no Received header from the receiving host",
        )];
    };

    let mut trusted: Option<(String, String)> = None;
    let mut forged: Vec<usize> = Vec::new();
    for (idx, (name, value)) in input.headers.iter().enumerate() {
        if !name.eq_ignore_ascii_case("authentication-results") {
            continue;
        }
        let Some(authserv) = authserv_id(value) else {
            continue;
        };
        if registrable(&authserv) != receiving_domain {
            continue;
        }
        if idx < boundary && trusted.is_none() {
            trusted = Some((authserv, value.clone()));
        } else {
            forged.push(idx);
        }
    }

    let mut signals = Vec::new();
    if !forged.is_empty() {
        signals.push(Signal::new(
            "ar_forged",
            AR_FORGED,
            format!(
                "{} Authentication-Results header(s) claim {receiving_domain} outside the receiving host's block",
                forged.len()
            ),
        ));
    }

    let Some((authserv, value)) = trusted else {
        signals.push(Signal::new(
            "auth_unverifiable",
            AUTH_UNVERIFIABLE,
            format!("no Authentication-Results from {receiving_domain}"),
        ));
        return signals;
    };

    let results = method_results(&value);
    let has = |method: &str, result: &str| results.iter().any(|(m, r)| m == method && r == result);
    if has("dmarc", "fail") {
        signals.push(Signal::new(
            "dmarc_fail",
            DMARC_FAIL,
            format!("authserv-id {authserv}: dmarc=fail"),
        ));
    }
    if has("spf", "fail") {
        signals.push(Signal::new(
            "spf_fail",
            SPF_FAIL,
            format!("authserv-id {authserv}: spf=fail"),
        ));
    } else if has("spf", "softfail") {
        signals.push(Signal::new(
            "spf_softfail",
            SPF_SOFTFAIL,
            format!("authserv-id {authserv}: spf=softfail"),
        ));
    }
    if has("dkim", "fail") && !has("dkim", "pass") {
        signals.push(Signal::new(
            "dkim_fail",
            DKIM_FAIL,
            format!("authserv-id {authserv}: dkim=fail"),
        ));
    }
    signals
}

/// The authserv-id: the first token before `;`, comments stripped, optional
/// version number dropped.
pub fn authserv_id(value: &str) -> Option<String> {
    let head = strip_comments(value.split(';').next()?);
    let id = head.split_whitespace().next()?.trim().trim_end_matches('.');
    (!id.is_empty()).then(|| id.to_lowercase())
}

/// `(method, result)` pairs after the authserv-id, lowercased.
pub fn method_results(value: &str) -> Vec<(String, String)> {
    let cleaned = strip_comments(value);
    cleaned
        .split(';')
        .skip(1)
        .filter_map(|clause| {
            let clause = clause.trim();
            let (method, rest) = clause.split_once('=')?;
            let method = method.trim().to_lowercase();
            let result = rest.split_whitespace().next()?.trim().to_lowercase();
            Some((method, result))
        })
        .collect()
}

fn strip_comments(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut depth = 0usize;
    for c in value.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{codes, input_from};
    use super::*;

    const RECEIVED_EDGE: &str = "Received: from mail.sender.example (mail.sender.example [203.0.113.5]) by mx1.example.org with ESMTPS id abc; Mon, 21 Sep 2026 10:00:00 +0000";
    const RECEIVED_INTERNAL: &str = "Received: from mx1.example.org by mailstore.example.org with LMTP id def; Mon, 21 Sep 2026 10:00:01 +0000";
    const RECEIVED_SENDER: &str = "Received: from laptop by mail.sender.example with ESMTPSA id ghi; Mon, 21 Sep 2026 09:59:59 +0000";

    #[test]
    fn genuine_pass_above_the_receiving_block_yields_no_signals() {
        let input = input_from(
            &[
                "Authentication-Results: mx1.example.org; spf=pass smtp.mailfrom=sender.example; dkim=pass header.d=sender.example; dmarc=pass header.from=sender.example",
                RECEIVED_INTERNAL,
                RECEIVED_EDGE,
                RECEIVED_SENDER,
                "From: a@sender.example",
            ],
            "hi",
        );
        assert!(analyze(&input).is_empty(), "{:?}", analyze(&input));
    }

    #[test]
    fn dmarc_and_spf_failures_from_the_trusted_header_count() {
        let input = input_from(
            &[
                "Authentication-Results: mx1.example.org (Postfix); spf=fail (sender IP is 198.51.100.7) smtp.mailfrom=bank.example; dkim=none; dmarc=fail (p=reject) header.from=bank.example",
                RECEIVED_EDGE,
                "From: a@bank.example",
            ],
            "hi",
        );
        let signals = analyze(&input);
        assert_eq!(codes(&signals), vec!["dmarc_fail", "spf_fail"]);
        assert_eq!(
            signals.iter().map(|s| s.weight).sum::<u32>(),
            DMARC_FAIL + SPF_FAIL
        );
    }

    #[test]
    fn foreign_authserv_id_is_ignored_even_when_it_claims_pass() {
        // Sender-supplied A-R under a different authserv-id: not ours, not trusted.
        let input = input_from(
            &[
                RECEIVED_EDGE,
                "Authentication-Results: mx.attacker.example; spf=pass; dkim=pass; dmarc=pass",
                RECEIVED_SENDER,
                "From: a@bank.example",
            ],
            "hi",
        );
        assert_eq!(codes(&analyze(&input)), vec!["auth_unverifiable"]);
    }

    #[test]
    fn correct_authserv_id_below_the_received_line_is_forged() {
        // Refuter 2: the attacker writes our own authserv-id before submission.
        // It sits below the receiving host's Received line, so it is not trusted.
        let input = input_from(
            &[
                RECEIVED_INTERNAL,
                RECEIVED_EDGE,
                "Authentication-Results: mx1.example.org; spf=pass; dkim=pass; dmarc=pass",
                RECEIVED_SENDER,
                "From: a@bank.example",
            ],
            "hi",
        );
        let signals = analyze(&input);
        assert_eq!(codes(&signals), vec!["ar_forged", "auth_unverifiable"]);
        assert_eq!(signals[0].weight, AR_FORGED);
    }

    #[test]
    fn a_second_matching_header_under_the_genuine_one_is_forged_and_ignored() {
        let input = input_from(
            &[
                "Authentication-Results: mx1.example.org; spf=fail; dmarc=fail",
                RECEIVED_EDGE,
                "Authentication-Results: mx1.example.org; spf=pass; dkim=pass; dmarc=pass",
                "From: a@bank.example",
            ],
            "hi",
        );
        assert_eq!(
            codes(&analyze(&input)),
            vec!["ar_forged", "dmarc_fail", "spf_fail"]
        );
    }

    #[test]
    fn gmail_layout_with_opaque_internal_hop_is_trusted() {
        let input = input_from(
            &[
                "Received: by 2002:a05:6a10:8f0e:b0:5f1:1234 with SMTP id x; Mon, 21 Sep 2026 10:00:02 -0700",
                "Authentication-Results: mx.google.com; dkim=pass header.i=@sender.example; spf=pass; dmarc=pass (p=NONE) header.from=sender.example",
                "Received: from mail.sender.example (mail.sender.example. [203.0.113.5]) by mx.google.com with ESMTPS id y; Mon, 21 Sep 2026 10:00:01 -0700",
                "From: a@sender.example",
            ],
            "hi",
        );
        assert!(analyze(&input).is_empty(), "{:?}", analyze(&input));
    }

    #[test]
    fn no_received_header_is_unverifiable() {
        let input = input_from(
            &[
                "Authentication-Results: mx1.example.org; dmarc=pass",
                "From: a@b.example",
            ],
            "hi",
        );
        assert_eq!(codes(&analyze(&input)), vec!["auth_unverifiable"]);
    }

    #[test]
    fn authserv_id_parsing_strips_comments_and_version() {
        assert_eq!(
            authserv_id("mx.google.com 1; spf=pass").as_deref(),
            Some("mx.google.com")
        );
        assert_eq!(
            authserv_id(" (comment) mx1.example.org; dkim=pass").as_deref(),
            Some("mx1.example.org")
        );
        assert_eq!(
            method_results("x; spf=pass (ok) smtp.mailfrom=a; dkim=fail"),
            vec![
                ("spf".to_string(), "pass".to_string()),
                ("dkim".to_string(), "fail".to_string())
            ]
        );
    }
}
