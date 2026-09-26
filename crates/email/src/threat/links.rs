// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Link analyzer: anchor text that names one host while the href goes to
//! another, IP-literal and punycode link hosts, look-alike link domains,
//! script/data hrefs, and forms that collect input inside the mail.
//!
//! The anchor scan is a tolerant regex pass over the HTML part, not a DOM:
//! it only has to find `<a href>` pairs well enough to compare hosts, and a
//! miss costs a signal, never a false clean on its own.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

use super::domains::{has_punycode, lookalike_of, registrable, to_unicode};
use super::{Signal, ThreatInput};

pub const ANCHOR_MISMATCH: u32 = 35;
pub const LOOKALIKE_LINK: u32 = 40;
pub const SCRIPT_LINK: u32 = 25;
pub const IP_LINK: u32 = 20;
pub const PUNYCODE_LINK: u32 = 15;
pub const PASSWORD_FIELD: u32 = 30;
pub const FORM_IN_BODY: u32 = 15;

/// Hosts listed per signal before the evidence is truncated.
const EVIDENCE_HOSTS: usize = 3;

static ANCHOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<a\b([^>]*)>(.*?)</a\s*>").expect("anchor regex"));
static HREF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).expect("href regex")
});
static NUMERIC_ENTITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&#([xX][0-9a-fA-F]{1,6}|[0-9]{1,7});").expect("entity regex"));
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").expect("tag regex"));
static TEXT_HOST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:https?://)?((?:[a-z0-9\x{80}-\x{10FFFF}-]+\.)+[a-z\x{80}-\x{10FFFF}]{2,})(?:[/:?#\s]|$)")
        .expect("text host regex")
});
static PLAIN_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bhttps?://[^\s<>"')]+"#).expect("plain url regex"));
static FORM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<form\b").expect("form regex"));
static PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<input\b[^>]*\btype\s*=\s*["']?password"#).expect("password regex")
});

/// One `<a>` from the HTML part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub href: String,
    pub text: String,
}

pub fn anchors(html: &str) -> Vec<Anchor> {
    ANCHOR
        .captures_iter(html)
        .filter_map(|cap| {
            let attrs = cap.get(1)?.as_str();
            let href = HREF.captures(attrs).and_then(|h| {
                h.get(1)
                    .or_else(|| h.get(2))
                    .or_else(|| h.get(3))
                    .map(|m| decode_entities(m.as_str().trim()))
            })?;
            let inner = cap.get(2).map(|m| m.as_str()).unwrap_or_default();
            let text = decode_entities(TAG.replace_all(inner, " ").trim());
            Some(Anchor { href, text })
        })
        .collect()
}

fn decode_entities(s: &str) -> String {
    let numeric = NUMERIC_ENTITY.replace_all(s, |cap: &regex::Captures<'_>| {
        let raw = &cap[1];
        let code = match raw.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => raw.parse::<u32>().ok(),
        };
        code.and_then(char::from_u32)
            .map(String::from)
            .unwrap_or_default()
    });
    numeric
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

fn scheme_of(href: &str) -> Option<String> {
    let compact: String = href
        .chars()
        .filter(|c| !c.is_control() && !c.is_whitespace())
        .collect();
    let (scheme, _) = compact.split_once(':')?;
    scheme
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        .then(|| scheme.to_ascii_lowercase())
}

fn http_host(href: &str) -> Option<String> {
    let url = url::Url::parse(href.trim()).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    url.host_str()
        .map(|h| h.trim_matches(['[', ']']).to_lowercase())
}

/// A host named in anchor text, e.g. "www.bank.example" or
/// "https://bank.example/login".
fn text_host(text: &str) -> Option<String> {
    let cap = TEXT_HOST.captures(text.trim())?;
    let host = cap.get(1)?.as_str().to_lowercase();
    // "Click.here" style tokens are not hosts; require a real-looking TLD.
    let tld = host.rsplit('.').next().unwrap_or_default();
    (tld.len() >= 2 && tld.chars().all(|c| c.is_alphabetic())).then_some(host)
}

/// Hosts of every http(s) link, in order: anchors in the HTML part, then
/// bare URLs in the text part.
pub fn link_hosts(input: &ThreatInput) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    if let Some(html) = &input.html {
        hosts.extend(anchors(html).iter().filter_map(|a| http_host(&a.href)));
    }
    if let Some(text) = &input.text {
        hosts.extend(
            PLAIN_URL
                .find_iter(text)
                .filter_map(|m| http_host(m.as_str())),
        );
    }
    hosts
}

#[derive(Default)]
struct Found {
    mismatch: BTreeSet<String>,
    lookalike: BTreeSet<String>,
    script: BTreeSet<String>,
    ip: BTreeSet<String>,
    punycode: BTreeSet<String>,
}

fn evidence(set: &BTreeSet<String>) -> String {
    let mut shown: Vec<&str> = set
        .iter()
        .take(EVIDENCE_HOSTS)
        .map(String::as_str)
        .collect();
    if set.len() > EVIDENCE_HOSTS {
        shown.push("…");
    }
    shown.join(", ")
}

pub fn analyze(input: &ThreatInput) -> Vec<Signal> {
    let known = input.known_domains();
    let mut found = Found::default();

    let mut hosts: Vec<String> = Vec::new();
    if let Some(html) = &input.html {
        for anchor in anchors(html) {
            if let Some(scheme) = scheme_of(&anchor.href)
                && matches!(scheme.as_str(), "javascript" | "vbscript" | "data" | "file")
            {
                found.script.insert(format!("{scheme}:"));
                continue;
            }
            let Some(href_host) = http_host(&anchor.href) else {
                continue;
            };
            if let Some(shown) = text_host(&anchor.text)
                && registrable(&shown) != registrable(&href_host)
            {
                found
                    .mismatch
                    .insert(format!("text {shown} -> {href_host}"));
            }
            hosts.push(href_host);
        }
    }
    if let Some(text) = &input.text {
        hosts.extend(
            PLAIN_URL
                .find_iter(text)
                .filter_map(|m| http_host(m.as_str())),
        );
    }

    for host in &hosts {
        if host.parse::<std::net::IpAddr>().is_ok() {
            found.ip.insert(host.clone());
            continue;
        }
        if has_punycode(host) {
            found
                .punycode
                .insert(format!("{host} ({})", to_unicode(host)));
        }
        if let Some(target) = lookalike_of(host, &known) {
            found.lookalike.insert(format!("{host} imitates {target}"));
        }
    }

    let mut signals = Vec::new();
    let mut push = |set: &BTreeSet<String>, code: &str, weight: u32| {
        if !set.is_empty() {
            signals.push(Signal::new(code, weight, evidence(set)));
        }
    };
    push(&found.mismatch, "anchor_mismatch", ANCHOR_MISMATCH);
    push(&found.lookalike, "lookalike_link", LOOKALIKE_LINK);
    push(&found.script, "script_link", SCRIPT_LINK);
    push(&found.ip, "ip_link", IP_LINK);
    push(&found.punycode, "punycode_link", PUNYCODE_LINK);

    if let Some(html) = &input.html {
        if PASSWORD.is_match(html) {
            signals.push(Signal::new(
                "password_field",
                PASSWORD_FIELD,
                "password input inside the message",
            ));
        } else if FORM.is_match(html) {
            signals.push(Signal::new(
                "form_in_body",
                FORM_IN_BODY,
                "form inside the message",
            ));
        }
    }
    signals
}

#[cfg(test)]
mod tests {
    use super::super::CorrespondentFacts;
    use super::super::test_support::{codes, input_from};
    use super::*;

    fn html_input(html: &str) -> ThreatInput {
        input_from(
            &[
                "From: a@sender.example",
                "MIME-Version: 1.0",
                "Content-Type: text/html; charset=utf-8",
            ],
            html,
        )
    }

    #[test]
    fn anchor_text_host_differing_from_href_host_is_flagged() {
        let input = html_input(
            r#"<p>Sign in at <a href="https://login.evil.example/x?y=1">https://www.mybank.example/login</a></p>"#,
        );
        let signals = analyze(&input);
        assert_eq!(codes(&signals), vec!["anchor_mismatch"]);
        assert_eq!(
            signals[0].evidence,
            "text www.mybank.example -> login.evil.example"
        );
    }

    #[test]
    fn same_registrable_domain_and_plain_words_are_not_mismatches() {
        let input = html_input(
            r#"<a href="https://links.mybank.example/t/1">www.mybank.example</a>
               <a href="https://news.example/a">Read more</a>
               <a href='mailto:help@mybank.example'>help@mybank.example</a>"#,
        );
        assert!(analyze(&input).is_empty(), "{:?}", analyze(&input));
    }

    #[test]
    fn ip_punycode_script_and_lookalike_links() {
        let spoof = idna::domain_to_ascii("p\u{0430}ypal.com").unwrap();
        let mut input = html_input(&format!(
            r#"<a href="http://192.0.2.10/login">Login</a>
               <a href="https://{spoof}/verify">Verify</a>
               <a href="java&#x0A;script:alert(1)">x</a>
               <a href="https://paypa1.com/">Account</a>"#
        ));
        input.ledger = Ok(CorrespondentFacts {
            known_domains: vec!["paypal.com".to_string()],
            ..CorrespondentFacts::default()
        });
        let signals = analyze(&input);
        assert_eq!(
            codes(&signals),
            vec!["lookalike_link", "script_link", "ip_link", "punycode_link"]
        );
        assert!(
            signals[0]
                .evidence
                .contains("paypa1.com imitates paypal.com"),
            "{}",
            signals[0].evidence
        );
    }

    #[test]
    fn password_form_outranks_plain_form() {
        let input =
            html_input(r#"<form action="https://x.example"><input type="password"></form>"#);
        assert_eq!(codes(&analyze(&input)), vec!["password_field"]);
        let input = html_input(r#"<form action="https://x.example"><input name="q"></form>"#);
        assert_eq!(codes(&analyze(&input)), vec!["form_in_body"]);
    }

    #[test]
    fn plain_text_urls_are_checked_for_ip_hosts() {
        let input = input_from(
            &["From: a@sender.example"],
            "Reset here: http://198.51.100.4/reset now",
        );
        assert_eq!(codes(&analyze(&input)), vec!["ip_link"]);
    }
}
