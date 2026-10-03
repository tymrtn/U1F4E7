// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Authentication-Results (RFC 8601) analyzer.
//!
//! Only results written by the receiving host count. RFC 8601 §5 says the
//! receiving MTA should strip pre-existing headers carrying its own
//! authserv-id, but many do not, so header *order* is the defence.
//!
//! The receiving host is named by the topmost `Received` with a DNS `by`
//! host; the receiver writes that line, and nothing a sender supplies can sit
//! above it. Results with the receiving domain's authserv-id above that line
//! are the receiver's own: an `Authentication-Results`, or Gmail's
//! `ARC-Authentication-Results`, since Gmail writes its plain A-R below its
//! edge `Received`.
//!
//! Below that line it is ambiguous. Gmail and Migadu write their A-R there,
//! but when a receiver writes none, a copy the sender put in the message also
//! sits there, with no `Received` of the sender's own when it connected
//! straight to the MX. So the topmost receiving-domain A-R between that line
//! and the first `Received` written by another domain counts for failures
//! only: a failure there is the receiver's or harms only the sender, and a
//! pass there proves nothing. Any other A-R claiming the receiving domain
//! (below a foreign hop, below the receiver's own results, or a second one)
//! was supplied by someone else: `ar_forged`. A-R headers from other
//! authserv-ids are ignored entirely.

use super::domains::registrable;
use super::{Signal, ThreatInput, is_dns_name, received_by_host};

pub const AR_FORGED: u32 = 40;
pub const DMARC_FAIL: u32 = 35;
pub const SPF_FAIL: u32 = 15;
pub const SPF_SOFTFAIL: u32 = 8;
pub const DKIM_FAIL: u32 = 12;
pub const AUTH_UNVERIFIABLE: u32 = 5;

/// The receiving host's Authentication-Results, separated from copies that
/// claim its authserv-id from anywhere else.
struct ReceivingAr {
    receiving_domain: String,
    /// The one set of results that is used, if any.
    trusted: Option<TrustedAr>,
    /// Other headers claiming the receiving domain.
    forged: usize,
}

struct TrustedAr {
    authserv_id: String,
    /// The header value, starting at the authserv-id.
    value: String,
    /// Above the receiving host's first `Received`, so the receiver wrote it.
    /// Otherwise it may be the sender's: its failures count, its passes do not.
    above_receiver: bool,
}

/// `None` when no Received header names a receiving host.
fn trusted_ar(input: &ThreatInput) -> Option<ReceivingAr> {
    let receiving_domain = registrable(input.receiving_host.as_deref()?);
    let is_received = |name: &str| name.eq_ignore_ascii_case("received");
    let dns_by_host = |value: &str| received_by_host(value).filter(|host| is_dns_name(host));

    // The receiver's first Received: the one `receiving_host` comes from.
    let anchor = input
        .headers
        .iter()
        .position(|(name, value)| is_received(name) && dns_by_host(value).is_some())?;
    // The first Received written by another domain. Opaque internal hops like
    // `by 2002:a05:...` do not end the receiving run.
    let boundary = input
        .headers
        .iter()
        .position(|(name, value)| {
            is_received(name)
                && matches!(dns_by_host(value), Some(host) if registrable(&host) != receiving_domain)
        })
        .unwrap_or(input.headers.len());

    let mut above: Option<(String, String, bool)> = None;
    let mut below: Option<(String, String)> = None;
    let mut forged = 0;
    for (idx, (name, value)) in input.headers.iter().enumerate() {
        let arc = name.eq_ignore_ascii_case("arc-authentication-results");
        if !arc && !name.eq_ignore_ascii_case("authentication-results") {
            continue;
        }
        // An ARC header starts with its instance, `i=N;`, before the authserv-id.
        let value = if arc {
            value.split_once(';').map_or("", |(_, rest)| rest.trim())
        } else {
            value.as_str()
        };
        let Some(authserv) = authserv_id(value) else {
            continue;
        };
        if registrable(&authserv) != receiving_domain {
            continue;
        }
        if idx < anchor {
            if above.is_none() {
                above = Some((authserv, value.to_string(), arc));
            }
            continue;
        }
        // ARC sets below the receiver's lines arrived with the message.
        if arc {
            continue;
        }
        // Gmail's own A-R sits below its edge line under its ARC results; a
        // receiver whose plain A-R is above the line wrote nothing below it.
        let receiver_ar_above = above.as_ref().is_some_and(|(_, _, arc)| !arc);
        if idx < boundary && below.is_none() && !receiver_ar_above {
            below = Some((authserv, value.to_string()));
        } else {
            forged += 1;
        }
    }

    let trusted = match (above, below) {
        (Some((authserv_id, value, _)), _) => Some(TrustedAr {
            authserv_id,
            value,
            above_receiver: true,
        }),
        (None, Some((authserv_id, value))) => Some(TrustedAr {
            authserv_id,
            value,
            above_receiver: false,
        }),
        (None, None) => None,
    };
    Some(ReceivingAr {
        receiving_domain,
        trusted,
        forged,
    })
}

pub fn analyze(input: &ThreatInput) -> Vec<Signal> {
    let Some(ReceivingAr {
        receiving_domain,
        trusted,
        forged,
    }) = trusted_ar(input)
    else {
        return vec![Signal::new(
            "auth_unverifiable",
            AUTH_UNVERIFIABLE,
            "no Received header names a receiving host",
        )];
    };

    let mut signals = Vec::new();
    if forged > 0 {
        signals.push(Signal::new(
            "ar_forged",
            AR_FORGED,
            format!(
                "{forged} Authentication-Results header(s) claim {receiving_domain} outside the receiving host's block"
            ),
        ));
    }

    let Some(TrustedAr {
        authserv_id: authserv,
        value,
        above_receiver,
    }) = trusted
    else {
        signals.push(Signal::new(
            "auth_unverifiable",
            AUTH_UNVERIFIABLE,
            format!("no Authentication-Results from {receiving_domain}"),
        ));
        return signals;
    };
    let before = signals.len();

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
    if !above_receiver && signals.len() == before {
        signals.push(Signal::new(
            "auth_unverifiable",
            AUTH_UNVERIFIABLE,
            format!(
                "the Authentication-Results from {authserv} sit below the receiving host's first Received, where the sender could have written them"
            ),
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

/// Whether the message's From domain is authenticated, by the receiving
/// host's own Authentication-Results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderAuth {
    /// `via` is `dmarc` or `dkim`; `domain` is the From domain it covers.
    Pass {
        via: String,
        domain: String,
        authserv_id: String,
    },
    /// The receiving host evaluated the message and the From domain did not
    /// pass, or the message does not have exactly one From mailbox.
    Fail,
    /// No result from the receiving host can be trusted.
    Unverifiable(String),
}

/// Authenticate the From domain.
///
/// Pass requires the trusted Authentication-Results to show `dmarc=pass` for
/// `header.from=<From domain>`, or `dkim=pass` with a signing domain
/// (`header.d`, else `header.i`) relaxed-aligned with the From domain (the
/// same registrable domain). SPF alone never counts: it covers the envelope
/// sender, not the From header. A message without exactly one From mailbox
/// fails.
pub fn sender_auth(input: &ThreatInput) -> SenderAuth {
    let Some(from_domain) = single_from_domain(input) else {
        return SenderAuth::Fail;
    };
    let Some(receiving) = trusted_ar(input) else {
        return SenderAuth::Unverifiable("no Received header names a receiving host".to_string());
    };
    let Some(TrustedAr {
        authserv_id,
        value,
        above_receiver,
    }) = receiving.trusted
    else {
        return SenderAuth::Unverifiable(format!(
            "no trusted Authentication-Results from {}",
            receiving.receiving_domain
        ));
    };

    let clauses = method_clauses(&value);
    let passed = |method: &str, covers: &dyn Fn(&MethodClause) -> bool| {
        clauses
            .iter()
            .any(|c| c.method == method && c.result == "pass" && covers(c))
    };
    let via = if passed("dmarc", &|c| {
        c.header_from.as_deref() == Some(from_domain.as_str())
    }) {
        "dmarc"
    } else if passed("dkim", &|c| {
        c.signing_domain()
            .is_some_and(|d| registrable(&d) == registrable(&from_domain))
    }) {
        "dkim"
    } else {
        return SenderAuth::Fail;
    };
    if !above_receiver {
        return SenderAuth::Unverifiable(format!(
            "the passing Authentication-Results from {authserv_id} sit below the receiving host's first Received, where the sender could have written them"
        ));
    }
    SenderAuth::Pass {
        via: via.to_string(),
        domain: from_domain,
        authserv_id,
    }
}

/// The domain of the message's only From mailbox, lowercased. `None` when
/// there is no From header, more than one, or it lists other than one mailbox.
fn single_from_domain(input: &ThreatInput) -> Option<String> {
    let mut from_headers = input
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("from"));
    let (_, value) = from_headers.next()?;
    if from_headers.next().is_some() {
        return None;
    }
    let raw = format!("From: {value}\r\n\r\n");
    let parsed = mail_parser::MessageParser::default().parse(raw.as_bytes())?;
    let mut mailboxes = parsed.from()?.iter();
    let mailbox = mailboxes.next()?;
    if mailboxes.next().is_some() {
        return None;
    }
    let (_, domain) = mailbox.address.as_deref()?.trim().rsplit_once('@')?;
    let domain = domain.trim().trim_end_matches('.').to_lowercase();
    (!domain.is_empty()).then_some(domain)
}

/// One `method=result` clause of an Authentication-Results header with the
/// properties sender authentication needs, lowercased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodClause {
    pub method: String,
    pub result: String,
    pub header_from: Option<String>,
    pub header_d: Option<String>,
    pub header_i: Option<String>,
}

impl MethodClause {
    /// The DKIM signing domain: `header.d`, or the domain of `header.i` when
    /// the receiver reported only that.
    fn signing_domain(&self) -> Option<String> {
        if let Some(d) = &self.header_d {
            return Some(d.clone());
        }
        let identity = self.header_i.as_deref()?;
        let domain = identity.rsplit_once('@').map_or(identity, |(_, d)| d);
        (!domain.is_empty()).then(|| domain.to_string())
    }
}

/// The clauses after the authserv-id. Comments and quoted strings (such as a
/// `reason`) are dropped first, so neither can supply a property.
pub fn method_clauses(value: &str) -> Vec<MethodClause> {
    strip_comments_and_quotes(value)
        .split(';')
        .skip(1)
        .filter_map(|clause| {
            let mut tokens = clause.split_whitespace();
            let (method, result) = tokens.next()?.split_once('=')?;
            let method = method.split('/').next()?.trim().to_lowercase();
            let mut parsed = MethodClause {
                method,
                result: result.trim().to_lowercase(),
                header_from: None,
                header_d: None,
                header_i: None,
            };
            for token in tokens {
                let Some((property, value)) = token.split_once('=') else {
                    continue;
                };
                let value = Some(value.trim().trim_end_matches('.').to_lowercase())
                    .filter(|v| !v.is_empty());
                match property.to_lowercase().as_str() {
                    "header.from" => parsed.header_from = value,
                    "header.d" => parsed.header_d = value,
                    "header.i" => parsed.header_i = value,
                    _ => {}
                }
            }
            Some(parsed)
        })
        .collect()
}

/// [`strip_comments`] that also drops quoted strings and leaves parentheses
/// inside quotes alone.
fn strip_comments_and_quotes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for c in value.chars() {
        if quoted {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' if depth == 0 => quoted = true,
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
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
    fn migadu_pass_below_its_own_received_is_unverifiable() {
        // Migadu writes its A-R below its own Received lines, where a sender's
        // copy would also sit when Migadu writes none. A pass there proves
        // nothing; it is not treated as forged either.
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
        assert_eq!(codes(&analyze(&input)), vec!["auth_unverifiable"]);
        assert!(matches!(sender_auth(&input), SenderAuth::Unverifiable(_)));
    }

    #[test]
    fn migadu_pass_without_any_upstream_received_is_unverifiable() {
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
        assert_eq!(codes(&analyze(&input)), vec!["auth_unverifiable"]);
        assert!(matches!(sender_auth(&input), SenderAuth::Unverifiable(_)));
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

    // ── sender_auth: is the From domain authenticated? ──────────────

    fn auth_of(headers: &[&str]) -> SenderAuth {
        sender_auth(&input_from(headers, "Your code is 123456"))
    }

    fn passed(via: &str, domain: &str, authserv_id: &str) -> SenderAuth {
        SenderAuth::Pass {
            via: via.to_string(),
            domain: domain.to_string(),
            authserv_id: authserv_id.to_string(),
        }
    }

    #[test]
    fn dmarc_pass_for_from_domain() {
        let auth = auth_of(&[
            "Authentication-Results: mx1.example.org; spf=pass smtp.mailfrom=bounce.bank.example; dkim=none; dmarc=pass (p=reject) header.from=bank.example",
            RECEIVED_EDGE,
            "From: Bank <alerts@bank.example>",
        ]);
        assert_eq!(auth, passed("dmarc", "bank.example", "mx1.example.org"));
    }

    #[test]
    fn dmarc_pass_for_other_header_from_fails() {
        let auth = auth_of(&[
            "Authentication-Results: mx1.example.org; dkim=pass header.d=attacker.example; dmarc=pass header.from=attacker.example",
            RECEIVED_EDGE,
            "From: Bank <alerts@bank.example>",
        ]);
        assert_eq!(auth, SenderAuth::Fail);
    }

    #[test]
    fn relaxed_dkim_alignment_passes() {
        let auth = auth_of(&[
            "Authentication-Results: mx1.example.org; dkim=pass header.d=mail.bank.example header.s=s1; spf=none; dmarc=none",
            RECEIVED_EDGE,
            "From: alerts@bank.example",
        ]);
        assert_eq!(auth, passed("dkim", "bank.example", "mx1.example.org"));

        // Gmail reports the signing domain as header.i.
        let auth = auth_of(&[
            "Received: by 2002:a05:6a10:8f0e:b0:5f1:1234 with SMTP id x; Mon, 21 Sep 2026 10:00:02 -0700",
            "Authentication-Results: mx.google.com; dkim=pass header.i=@bank.example; spf=softfail",
            "Received: from mail.bank.example (mail.bank.example. [203.0.113.5]) by mx.google.com with ESMTPS id y; Mon, 21 Sep 2026 10:00:01 -0700",
            "From: alerts@login.bank.example",
        ]);
        assert_eq!(auth, passed("dkim", "login.bank.example", "mx.google.com"));
    }

    #[test]
    fn unaligned_dkim_fails() {
        let auth = auth_of(&[
            "Authentication-Results: mx1.example.org; dkim=pass header.d=bulk-sender.example; spf=pass smtp.mailfrom=bulk-sender.example; dmarc=none header.from=bank.example",
            RECEIVED_EDGE,
            "From: alerts@bank.example",
        ]);
        assert_eq!(auth, SenderAuth::Fail);

        // header.d is the signing domain; an aligned header.i does not override it.
        let auth = auth_of(&[
            "Authentication-Results: mx1.example.org; dkim=pass header.d=bulk-sender.example header.i=@bank.example",
            RECEIVED_EDGE,
            "From: alerts@bank.example",
        ]);
        assert_eq!(auth, SenderAuth::Fail);
    }

    #[test]
    fn spf_pass_alone_fails() {
        let auth = auth_of(&[
            "Authentication-Results: mx1.example.org; spf=pass smtp.mailfrom=bank.example; dkim=none; dmarc=none header.from=bank.example",
            RECEIVED_EDGE,
            "From: alerts@bank.example",
        ]);
        assert_eq!(auth, SenderAuth::Fail);
    }

    #[test]
    fn pass_only_in_forged_header_is_unverifiable() {
        let auth = auth_of(&[
            MIGADU_LMTP,
            "Received: from mail.spoofer.example (198.51.100.7) by mizu0.migadu.com with ESMTPS id q6; Mon, 28 Sep 2026 04:43:18 +0000",
            "Received: from laptop (unknown [192.0.2.44]) by mail.spoofer.example with ESMTPSA id q7; Mon, 28 Sep 2026 04:43:16 +0000",
            "Authentication-Results: mx13.migadu.com; dkim=pass header.d=bank.example; spf=pass; dmarc=pass header.from=bank.example",
            "From: Bank <alerts@bank.example>",
        ]);
        assert!(matches!(auth, SenderAuth::Unverifiable(_)), "{auth:?}");
    }

    #[test]
    fn no_trusted_ar_is_unverifiable() {
        let auth = auth_of(&[RECEIVED_EDGE, "From: alerts@bank.example"]);
        assert!(matches!(auth, SenderAuth::Unverifiable(_)), "{auth:?}");

        // A pass from another authserv-id is not the receiving host's word.
        let auth = auth_of(&[
            RECEIVED_EDGE,
            "Authentication-Results: mx.attacker.example; dkim=pass header.d=bank.example; dmarc=pass header.from=bank.example",
            "From: alerts@bank.example",
        ]);
        assert!(matches!(auth, SenderAuth::Unverifiable(_)), "{auth:?}");

        let auth = auth_of(&[
            "Authentication-Results: mx1.example.org; dmarc=pass header.from=bank.example",
            "From: alerts@bank.example",
        ]);
        assert!(matches!(auth, SenderAuth::Unverifiable(_)), "{auth:?}");
    }

    #[test]
    fn two_from_mailboxes_fails() {
        let pass = "Authentication-Results: mx1.example.org; dkim=pass header.d=bank.example; dmarc=pass header.from=bank.example";
        let auth = auth_of(&[
            pass,
            RECEIVED_EDGE,
            "From: alerts@bank.example",
            "From: attacker@evil.example",
        ]);
        assert_eq!(auth, SenderAuth::Fail);

        let auth = auth_of(&[
            pass,
            RECEIVED_EDGE,
            "From: alerts@bank.example, attacker@evil.example",
        ]);
        assert_eq!(auth, SenderAuth::Fail);

        let auth = auth_of(&[pass, RECEIVED_EDGE, "Subject: no sender"]);
        assert_eq!(auth, SenderAuth::Fail);
    }

    // ── Trust boundary: only results above the receiver's first Received ──

    /// A real Gmail delivery, anonymized: Gmail's A-R sits below its own edge
    /// Received, and its ARC-Authentication-Results sits above it.
    fn gmail_delivery(arc_results: &str, ar_results: &str, extra_below: &[&str]) -> Vec<String> {
        let mut headers = vec![
            "Delivered-To: me@gmail.example".to_string(),
            "Received: by 2002:a05:6022:5c4:b0:9a1:1234 with SMTP id x1; Mon, 7 Oct 2026 08:12:34 -0700 (PDT)".to_string(),
            "X-Google-Smtp-Source: AGHT+IEexample".to_string(),
            "X-Received: by 2002:a05:6214:2a1:b0:6b2:9876 with SMTP id x2; Mon, 07 Oct 2026 08:12:34 -0700 (PDT)".to_string(),
            "ARC-Seal: i=1; a=rsa-sha256; t=1791378754; cv=none; d=google.com; s=arc-20240605; b=abc".to_string(),
            "ARC-Message-Signature: i=1; a=rsa-sha256; c=relaxed/relaxed; d=google.com; s=arc-20240605; bh=x; b=y".to_string(),
            format!("ARC-Authentication-Results: i=1; mx.google.com; {arc_results}"),
            "Return-Path: <noreply@bank.example>".to_string(),
            "Received: from out-18.smtp.bank.example (out-18.smtp.bank.example. [192.0.2.201]) by mx.google.com with ESMTPS id x3 for <me@gmail.example> (version=TLS1_3); Mon, 07 Oct 2026 08:12:34 -0700 (PDT)".to_string(),
            "Received-SPF: pass (google.com: domain of noreply@bank.example designates 192.0.2.201 as permitted sender) client-ip=192.0.2.201;".to_string(),
            format!("Authentication-Results: mx.google.com; {ar_results}"),
        ];
        headers.extend(extra_below.iter().map(|h| h.to_string()));
        headers.push("From: Bank <noreply@bank.example>".to_string());
        headers
    }

    fn auth_of_owned(headers: &[String]) -> SenderAuth {
        let refs: Vec<&str> = headers.iter().map(String::as_str).collect();
        auth_of(&refs)
    }

    const BANK_PASS: &str = "dkim=pass header.i=@bank.example header.s=s1 header.b=abc; spf=pass smtp.mailfrom=noreply@bank.example; dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=bank.example";
    const BANK_FAIL: &str = "dkim=none; spf=fail smtp.mailfrom=noreply@bank.example; dmarc=fail (p=REJECT) header.from=bank.example";

    #[test]
    fn direct_to_mx_injection_without_receiver_ar_is_unverifiable() {
        // The sender connected straight to the MX, so no Received of its own
        // sits between the receiver's lines and the A-R it supplied.
        let headers = [
            "Received: from mx1.example.org by store.example.org with LMTP id q; Mon, 21 Sep 2026 10:00:01 +0000",
            "Received: from swaks.attacker.example (198.51.100.7) by mx1.example.org with ESMTP id q2; Mon, 21 Sep 2026 10:00:00 +0000",
            "Authentication-Results: mx1.example.org; dkim=pass header.d=bank.example; dmarc=pass header.from=bank.example",
            "From: Bank <alerts@bank.example>",
        ];
        assert!(
            matches!(auth_of(&headers), SenderAuth::Unverifiable(_)),
            "{:?}",
            auth_of(&headers)
        );
        let input = input_from(&headers, "hi");
        assert_eq!(codes(&analyze(&input)), vec!["auth_unverifiable"]);
    }

    #[test]
    fn forged_ar_below_the_receivers_received_loses_to_the_real_one_above() {
        let headers = [
            "Authentication-Results: mx1.example.org; dkim=none; spf=fail smtp.mailfrom=bank.example; dmarc=fail (p=reject) header.from=bank.example",
            "Received: from mx1.example.org by store.example.org with LMTP id q; Mon, 21 Sep 2026 10:00:01 +0000",
            "Received: from swaks.attacker.example (198.51.100.7) by mx1.example.org with ESMTP id q2; Mon, 21 Sep 2026 10:00:00 +0000",
            "Authentication-Results: mx1.example.org; dkim=pass header.d=bank.example; dmarc=pass header.from=bank.example",
            "From: Bank <alerts@bank.example>",
        ];
        assert_eq!(auth_of(&headers), SenderAuth::Fail);
        let input = input_from(&headers, "hi");
        assert_eq!(
            codes(&analyze(&input)),
            vec!["ar_forged", "dmarc_fail", "spf_fail"]
        );

        // The same pair with a passing real result: the forged copy changes nothing.
        let headers = [
            "Authentication-Results: mx1.example.org; dkim=pass header.d=bank.example; dmarc=pass header.from=bank.example",
            headers[1],
            headers[2],
            "Authentication-Results: mx1.example.org; dmarc=pass header.from=attacker.example",
            headers[4],
        ];
        assert_eq!(
            auth_of(&headers),
            passed("dmarc", "bank.example", "mx1.example.org")
        );
    }

    #[test]
    fn gmail_ordering_passes_on_its_own_results() {
        let headers = gmail_delivery(
            BANK_PASS,
            BANK_PASS,
            &[
                "Received: from laptop (unknown [192.0.2.10]) by mail.bank.example with ESMTPSA id z; Mon, 07 Oct 2026 08:12:33 -0700",
            ],
        );
        assert_eq!(
            auth_of_owned(&headers),
            passed("dmarc", "bank.example", "mx.google.com")
        );
        let refs: Vec<&str> = headers.iter().map(String::as_str).collect();
        assert!(analyze(&input_from(&refs, "hi")).is_empty());

        // Gmail's own failing result wins over a passing copy the sender
        // placed under it.
        let headers = gmail_delivery(
            BANK_FAIL,
            BANK_FAIL,
            &[
                "Authentication-Results: mx.google.com; dkim=pass header.d=bank.example; dmarc=pass header.from=bank.example",
            ],
        );
        assert_eq!(auth_of_owned(&headers), SenderAuth::Fail);
        let refs: Vec<&str> = headers.iter().map(String::as_str).collect();
        assert_eq!(
            codes(&analyze(&input_from(&refs, "hi"))),
            vec!["ar_forged", "dmarc_fail", "spf_fail"]
        );
    }

    #[test]
    fn migadu_style_without_ar_is_unverifiable() {
        let headers = [
            "Delivered-To: alice@example.net",
            MIGADU_LMTP,
            "Received: from mail.bank.example (203.0.113.5) by mizu0.migadu.com with ESMTPS id q8; Mon, 28 Sep 2026 04:43:18 +0000",
            "From: Bank <alerts@bank.example>",
        ];
        assert!(matches!(auth_of(&headers), SenderAuth::Unverifiable(_)));
    }

    #[test]
    fn a_failure_below_the_receivers_received_still_fails() {
        // A failing result there is either the receiver's own or a sender
        // harming itself; both mean the code is refused.
        let headers = [
            MIGADU_LMTP,
            "Received: from mail.spoofer.example (198.51.100.7) by mizu0.migadu.com with ESMTPS id q3; Mon, 28 Sep 2026 04:43:18 +0000",
            "Authentication-Results: mx13.migadu.com; dkim=none; spf=fail smtp.mailfrom=bank.example; dmarc=fail (policy=reject) header.from=bank.example",
            "From: Bank <alerts@bank.example>",
        ];
        assert_eq!(auth_of(&headers), SenderAuth::Fail);
    }

    #[test]
    fn microsoft_ar_without_authserv_id_plus_injection_is_unverifiable() {
        let headers = [
            "Received: from DM6PR.namprd.prod.outlook.com (2603:10b6::1) by BN8PR.namprd.prod.outlook.com with HTTPS; Mon, 21 Sep 2026 10:00:02 +0000",
            "Authentication-Results: spf=fail (sender IP is 198.51.100.7) smtp.mailfrom=bank.example; dkim=none (message not signed) header.d=none;dmarc=fail action=quarantine header.from=bank.example;compauth=fail reason=000",
            "Received: from swaks.attacker.example (198.51.100.7) by BN1NAM02FT.mail.protection.outlook.com (10.0.0.1) with Microsoft SMTP Server; Mon, 21 Sep 2026 10:00:01 +0000",
            "Authentication-Results: mx.outlook.com; dmarc=pass header.from=bank.example",
            "From: Bank <alerts@bank.example>",
        ];
        assert!(matches!(auth_of(&headers), SenderAuth::Unverifiable(_)));
    }

    #[test]
    fn method_clauses_read_the_alignment_properties() {
        let clauses = method_clauses(
            "mx13.migadu.com; dkim=pass header.d=Sender.Example header.s=google header.b=OP5j; spf=pass (mx13: ok) smtp.mailfrom=bob@sender.example; dmarc=fail reason=\"SPF not aligned (relaxed), header.from=x.example\" header.from=example.net (policy=reject)",
        );
        assert_eq!(clauses.len(), 3);
        assert_eq!(clauses[0].method, "dkim");
        assert_eq!(clauses[0].result, "pass");
        assert_eq!(clauses[0].header_d.as_deref(), Some("sender.example"));
        assert_eq!(clauses[1].method, "spf");
        assert_eq!(clauses[1].header_from, None);
        assert_eq!(clauses[2].result, "fail");
        // A quoted reason never supplies a property.
        assert_eq!(clauses[2].header_from.as_deref(), Some("example.net"));
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
