// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Authentication-Results (RFC 8601) analyzer.
//!
//! Only an A-R header written by the receiving host counts. RFC 8601 §5 says
//! the receiving MTA should strip pre-existing headers carrying its own
//! authserv-id, but many do not, so header *order* is the defence. Everything
//! above the first `Received` line written by another domain was prepended
//! by the receiving domain. Where the provider puts its A-R relative to its
//! own `Received` lines varies: Gmail and Migadu write it *below* their edge
//! `Received`. So the boundary is the first foreign `Received` (or the end of
//! the block when there is none), and we trust only the topmost A-R whose
//! authserv-id belongs to the receiving domain and that appears above it.
//! Any other A-R claiming the receiving domain (below a foreign hop, or a
//! second one) was supplied by someone else: `ar_forged`. A-R headers from
//! other authserv-ids are ignored entirely.
//!
//! Header order matters here, so this reads the raw header list
//! ([`ThreatInput::headers`]), and only `Received` and
//! `Authentication-Results`: fields the receiving server adds, which the
//! content fingerprint leaves out by design.

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

    // Boundary: the first Received header written by another domain. Opaque
    // internal hops like `by 2002:a05:...` do not end the receiving run.
    let boundary = input
        .headers
        .iter()
        .position(|(name, value)| {
            name.eq_ignore_ascii_case("received")
                && matches!(received_by_host(value),
                    Some(host) if is_dns_name(&host) && registrable(&host) != receiving_domain)
        })
        .unwrap_or(input.headers.len());

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
        // The sender's own MTA then prepends its Received above it, so it sits
        // below another hop's Received and is not trusted.
        let input = input_from(
            &[
                RECEIVED_INTERNAL,
                RECEIVED_EDGE,
                RECEIVED_SENDER,
                "Authentication-Results: mx1.example.org; spf=pass; dkim=pass; dmarc=pass",
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

    // Migadu layout, taken from a real delivery (addresses anonymized): the
    // provider prepends its edge Received first and its A-R above that, then
    // its LMTP Received on top, so the genuine A-R sits BELOW the provider's
    // own Received lines.
    const MIGADU_LMTP: &str = "Received: from mizu1.migadu.com ([135.125.171.180]) by soraStorage0.migadu.com with LMTP id nk47az64b3j4m43x647a for <alice@example.net>; Mon, 28 Sep 2026 04:43:19 +0000";
    const MIGADU_EDGE: &str = "Received: from mail-ed1-x548.google.com (2a00:1450:4864:20::548) by mizu0.migadu.com with ESMTPS id 23d260237217360d; Mon, 28 Sep 2026 04:43:18 +0000";
    const MIGADU_AR_PASS: &str = "Authentication-Results: mx13.migadu.com; dkim=pass header.d=sender.example header.s=google header.b=OP5j4qFe; spf=pass (mx13.migadu.com: domain of bob@sender.example designates 2a00:1450:4864:20::548 as permitted sender) smtp.mailfrom=bob@sender.example; dmarc=pass (policy=none) header.from=sender.example";
    const GMAIL_SUBMISSION: &str = "Received: by mail-ed1-x548.google.com with SMTP id 4fb4d7f45d1cf-6a0a4a22bf0so3254826a12.0 for <alice@example.net>; Sun, 27 Sep 2026 21:43:14 -0700 (PDT)";

    #[test]
    fn migadu_layout_with_ar_below_its_own_received_is_trusted() {
        let input = input_from(
            &[
                "Delivered-To: alice@example.net",
                MIGADU_LMTP,
                MIGADU_EDGE,
                MIGADU_AR_PASS,
                GMAIL_SUBMISSION,
                "From: Bob <bob@sender.example>",
            ],
            "hi",
        );
        assert!(analyze(&input).is_empty(), "{:?}", analyze(&input));
    }

    #[test]
    fn migadu_layout_without_any_upstream_received_is_trusted() {
        // SES and many ESPs add no Received of their own.
        let input = input_from(
            &[
                "Delivered-To: alice@example.net",
                "Received: from mizu0.migadu.com ([51.38.57.138]) by soraStorage0.migadu.com with LMTP id q1 for <alice@example.net>; Sat, 26 Sep 2026 07:00:22 +0000",
                "Received: from a1-20.smtp-out.eu-west-1.amazonses.com (54.240.1.20) by mizu0.migadu.com with ESMTPS id q2; Sat, 26 Sep 2026 07:00:21 +0000",
                "Authentication-Results: mx13.migadu.com; dkim=pass header.d=amazonses.com; spf=pass smtp.mailfrom=bounces.shop.example; dmarc=pass (policy=quarantine) header.from=shop.example",
                "From: Shop <news@shop.example>",
            ],
            "hi",
        );
        assert!(analyze(&input).is_empty(), "{:?}", analyze(&input));
    }

    #[test]
    fn migadu_dmarc_failure_is_evaluated() {
        let input = input_from(
            &[
                MIGADU_LMTP,
                "Received: from mail.spoofer.example (198.51.100.7) by mizu0.migadu.com with ESMTPS id q3; Mon, 28 Sep 2026 04:43:18 +0000",
                "Authentication-Results: mx13.migadu.com; dkim=none; spf=fail (mx13.migadu.com: domain of it@example.net does not designate 198.51.100.7 as permitted sender) smtp.mailfrom=it@example.net; dmarc=fail reason=\"SPF not aligned (relaxed), No valid DKIM\" header.from=example.net (policy=reject)",
                "From: IT <it@example.net>",
            ],
            "hi",
        );
        assert_eq!(codes(&analyze(&input)), vec!["dmarc_fail", "spf_fail"]);
    }

    #[test]
    fn sender_inserted_migadu_ar_below_an_upstream_received_is_forged() {
        // The attacker writes Migadu's authserv-id into the message before
        // submission; their MSA's Received lands above it. Migadu's genuine
        // A-R (dmarc=fail) is still the one trusted.
        let input = input_from(
            &[
                MIGADU_LMTP,
                "Received: from mail.spoofer.example (198.51.100.7) by mizu0.migadu.com with ESMTPS id q4; Mon, 28 Sep 2026 04:43:18 +0000",
                "Authentication-Results: mx13.migadu.com; dkim=none; spf=pass smtp.mailfrom=bounce@spoofer.example; dmarc=fail (policy=reject) header.from=bank.example",
                "Received: from laptop (unknown [192.0.2.44]) by mail.spoofer.example with ESMTPSA id q5; Mon, 28 Sep 2026 04:43:16 +0000",
                "Authentication-Results: mx13.migadu.com; dkim=pass header.d=bank.example; spf=pass; dmarc=pass header.from=bank.example",
                "From: Bank <alerts@bank.example>",
            ],
            "hi",
        );
        let signals = analyze(&input);
        assert_eq!(codes(&signals), vec!["ar_forged", "dmarc_fail"]);
        assert_eq!(signals[0].weight, AR_FORGED);
    }

    #[test]
    fn lone_sender_inserted_ar_below_an_upstream_received_is_forged() {
        let input = input_from(
            &[
                MIGADU_LMTP,
                "Received: from mail.spoofer.example (198.51.100.7) by mizu0.migadu.com with ESMTPS id q6; Mon, 28 Sep 2026 04:43:18 +0000",
                "Received: from laptop (unknown [192.0.2.44]) by mail.spoofer.example with ESMTPSA id q7; Mon, 28 Sep 2026 04:43:16 +0000",
                "Authentication-Results: mx13.migadu.com; dkim=pass; spf=pass; dmarc=pass",
                "From: Bank <alerts@bank.example>",
            ],
            "hi",
        );
        assert_eq!(
            codes(&analyze(&input)),
            vec!["ar_forged", "auth_unverifiable"]
        );
    }

    #[test]
    fn gmail_layout_with_upstream_sender_hops_is_trusted() {
        let input = input_from(
            &[
                "Delivered-To: alice@example.com",
                "Received: by 2002:a05:6a10:8f0e:b0:5f1:1234 with SMTP id x; Mon, 21 Sep 2026 10:00:02 -0700",
                "X-Google-Smtp-Source: AGHT+IEexample",
                "ARC-Seal: i=1; a=rsa-sha256; t=1790000000; cv=none; d=google.com; s=arc-20240605; b=abc",
                "ARC-Authentication-Results: i=1; mx.google.com; dkim=pass header.i=@sender.example; spf=pass; dmarc=pass header.from=sender.example",
                "Return-Path: <bob@sender.example>",
                "Received: from mail.sender.example (mail.sender.example. [203.0.113.5]) by mx.google.com with ESMTPS id y; Mon, 21 Sep 2026 10:00:01 -0700",
                "Received-SPF: pass (google.com: domain of bob@sender.example designates 203.0.113.5 as permitted sender) client-ip=203.0.113.5;",
                "Authentication-Results: mx.google.com; dkim=pass header.i=@sender.example; spf=pass smtp.mailfrom=bob@sender.example; dmarc=pass (p=NONE) header.from=sender.example",
                "Received: from laptop (unknown [192.0.2.10]) by mail.sender.example with ESMTPSA id z; Mon, 21 Sep 2026 10:00:00 -0700",
                "From: Bob <bob@sender.example>",
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
