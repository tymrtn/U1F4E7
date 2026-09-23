// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! rShield: the local threat engine.
//!
//! Every analyzer is a pure `fn analyze(&ThreatInput) -> Vec<Signal>` behind
//! its own `threat.analyzers.<name>` flag. The combiner adds signal weights
//! (capped at 100) into a [`ThreatVerdict`]: `>= 30` is suspicious, `>= 70`
//! dangerous. A required analyzer that errors makes the verdict
//! `unavailable`, never `clean` — an engine that could not look must not vouch
//! for a message.
//!
//! Analyzers plug in through [`Analyzer`]. The local six ship here; clamd and
//! domain reputation (A5) and the Jev decision model (A6) implement the same
//! trait, may do I/O, and declare whether their failure is fatal through
//! [`Analyzer::required`].
//!
//! Signal evidence carries hosts, domains, extensions and hashes only — never
//! bodies, subjects or full URLs — because verdicts are stored in the
//! `threat_verdict` event payload.

pub mod attachments;
pub mod auth_results;
pub mod config;
pub mod content;
pub mod domains;
pub mod ledger;
pub mod links;
pub mod persist;
pub mod report;
pub mod sender;

use mail_parser::MimeHeaders;
use serde::{Deserialize, Serialize};

pub use config::{Quarantine, ThreatConfig};
pub use envelope_email_store::correspondents::CorrespondentFacts;

/// Bumped whenever an analyzer or weight changes; a stored verdict from an
/// older engine is rescanned on open.
pub const ENGINE_VERSION: &str = "rshield-1";

/// `message_scores.dimension` holding the verdict score, so `score_above
/// threat N` rules match without any new rule primitive.
pub const THREAT_DIMENSION: &str = "threat";

pub const TAG_SUSPICIOUS: &str = "threat:suspicious";
pub const TAG_DANGEROUS: &str = "threat:dangerous";
pub const TAG_MALWARE: &str = "threat:malware";
pub const TAG_QUARANTINED: &str = "threat:quarantined";
pub const TAG_FALSE_POSITIVE: &str = "threat:false_positive";

pub const SUSPICIOUS_THRESHOLD: u32 = 30;
pub const DANGEROUS_THRESHOLD: u32 = 70;
pub const MAX_SCORE: u32 = 100;

/// One piece of evidence an analyzer found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    /// Stable machine code, e.g. `lookalike_domain`.
    pub code: String,
    /// Points added to the score.
    pub weight: u32,
    /// Hosts, domains, extensions, hashes. Never bodies.
    pub evidence: String,
    /// Malware-grade: the message gets `threat:malware` and its attachments
    /// refuse download.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub malware: bool,
}

impl Signal {
    pub fn new(code: &str, weight: u32, evidence: impl Into<String>) -> Self {
        Signal {
            code: code.to_string(),
            weight,
            evidence: evidence.into(),
            malware: false,
        }
    }

    pub fn malware(mut self) -> Self {
        self.malware = true;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Clean,
    Suspicious,
    Dangerous,
    Unavailable,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Clean => "clean",
            Level::Suspicious => "suspicious",
            Level::Dangerous => "dangerous",
            Level::Unavailable => "unavailable",
        }
    }

    pub fn for_score(score: u32) -> Level {
        if score >= DANGEROUS_THRESHOLD {
            Level::Dangerous
        } else if score >= SUSPICIOUS_THRESHOLD {
            Level::Suspicious
        } else {
            Level::Clean
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedAnalyzer {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreatVerdict {
    pub score: u32,
    pub level: Level,
    pub signals: Vec<Signal>,
    pub analyzers_run: Vec<String>,
    pub analyzers_skipped: Vec<SkippedAnalyzer>,
    pub engine_version: String,
    pub computed_at: String,
}

impl ThreatVerdict {
    pub fn is_malware(&self) -> bool {
        self.signals.iter().any(|s| s.malware)
    }

    /// The verdict for a message the engine could not examine at all (raw
    /// bytes unparseable, config unreadable).
    pub fn unavailable(reason: impl Into<String>) -> Self {
        ThreatVerdict {
            score: 0,
            level: Level::Unavailable,
            signals: Vec::new(),
            analyzers_run: Vec::new(),
            analyzers_skipped: vec![SkippedAnalyzer {
                name: "input".to_string(),
                reason: reason.into(),
            }],
            engine_version: ENGINE_VERSION.to_string(),
            computed_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// One attachment as the engine sees it. `bytes` is `None` when only
/// metadata was fetched.
#[derive(Debug, Clone, Default)]
pub struct AttachmentInput {
    pub filename: String,
    pub content_type: String,
    pub size: u64,
    pub bytes: Option<Vec<u8>>,
}

/// Everything the analyzers look at, built once per message.
#[derive(Debug, Clone)]
pub struct ThreatInput {
    /// Header fields in wire order (top first), unfolded.
    pub headers: Vec<(String, String)>,
    /// Lowercased From address.
    pub from_addr: String,
    pub from_display: Option<String>,
    /// Lowercased Reply-To addresses.
    pub reply_to: Vec<String>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<AttachmentInput>,
    /// The mailbox this message was delivered to (lowercased).
    pub account_address: String,
    /// The host that accepted the message: the first DNS-named `by` host in
    /// the topmost `Received` headers.
    pub receiving_host: Option<String>,
    /// Correspondent data. `Err` when the local ledger could not be read;
    /// the ledger analyzer then fails and the verdict is `unavailable`.
    pub ledger: Result<CorrespondentFacts, String>,
}

impl ThreatInput {
    /// Parse raw RFC 5322 bytes. The ledger starts as not-loaded; callers
    /// fill it with [`persist::load_ledger`] or a fixture.
    pub fn from_raw(raw: &[u8], account_address: &str) -> Result<Self, String> {
        let parsed = mail_parser::MessageParser::default()
            .parse(raw)
            .ok_or_else(|| "message bytes could not be parsed".to_string())?;
        let headers = parse_header_block(raw);

        let (from_addr, from_display) = match parsed.from().and_then(|a| a.first()) {
            Some(addr) => (
                addr.address
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase(),
                addr.name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string),
            ),
            None => (String::new(), None),
        };
        let reply_to = parsed
            .reply_to()
            .map(|a| {
                a.iter()
                    .filter_map(|addr| addr.address.as_deref())
                    .map(|s| s.trim().to_lowercase())
                    .collect()
            })
            .unwrap_or_default();

        let attachments = parsed
            .attachments()
            .map(|part| {
                let content_type = part
                    .content_type()
                    .map(|ct| format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("octet-stream")))
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                AttachmentInput {
                    filename: part.attachment_name().unwrap_or("unnamed").to_string(),
                    content_type: content_type.to_lowercase(),
                    size: part.len() as u64,
                    bytes: Some(part.contents().to_vec()),
                }
            })
            .collect();

        let receiving_host = receiving_host(&headers);
        Ok(ThreatInput {
            headers,
            from_addr,
            from_display,
            reply_to,
            text: parsed.body_text(0).map(|t| t.to_string()),
            html: parsed.body_html(0).map(|h| h.to_string()),
            attachments,
            account_address: account_address.trim().to_lowercase(),
            receiving_host,
            ledger: Err("correspondent ledger not loaded".to_string()),
        })
    }

    /// Domains this mailbox already corresponds with, plus its own domain.
    pub fn known_domains(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(own) = domains::domain_of(&self.account_address) {
            out.push(own);
        }
        if let Ok(facts) = &self.ledger {
            out.extend(facts.known_domains.iter().cloned());
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn from_domain(&self) -> Option<String> {
        domains::domain_of(&self.from_addr)
    }
}

/// Header fields in wire order, continuation lines unfolded.
pub fn parse_header_block(raw: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(raw);
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = headers.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    headers
}

/// The `by` host of a `Received` header, if it names one.
pub fn received_by_host(value: &str) -> Option<String> {
    let lower = value.to_lowercase();
    let mut words = lower.split_whitespace();
    while let Some(word) = words.next() {
        if word == "by" {
            return words
                .next()
                .map(|h| h.trim_matches(|c: char| c == '(' || c == ')' || c == ';' || c == '['))
                .map(|h| h.trim_end_matches('.').to_string())
                .filter(|h| !h.is_empty());
        }
    }
    None
}

/// A `by` host that is a DNS name (not an IP literal or opaque id).
pub fn is_dns_name(host: &str) -> bool {
    host.contains('.')
        && host.chars().any(|c| c.is_ascii_alphabetic())
        && host.parse::<std::net::IpAddr>().is_err()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

fn receiving_host(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("received"))
        .filter_map(|(_, value)| received_by_host(value))
        .find(|host| is_dns_name(host))
}

/// A pluggable analyzer. Local analyzers are infallible pure functions; I/O
/// analyzers (clamd, reputation, Jev) return `Err` when they could not run.
pub trait Analyzer: Send + Sync {
    fn name(&self) -> &'static str;
    /// A required analyzer's error makes the verdict `unavailable`; an
    /// optional one's is recorded in `analyzers_skipped`.
    fn required(&self) -> bool {
        true
    }
    fn analyze(&self, input: &ThreatInput) -> Result<Vec<Signal>, String>;
}

struct Pure {
    name: &'static str,
    run: fn(&ThreatInput) -> Vec<Signal>,
}

impl Analyzer for Pure {
    fn name(&self) -> &'static str {
        self.name
    }
    fn analyze(&self, input: &ThreatInput) -> Result<Vec<Signal>, String> {
        Ok((self.run)(input))
    }
}

/// Names of the shipped analyzers, in run order. These are the valid
/// `threat.analyzers.<name>` keys.
pub const ANALYZER_NAMES: &[&str] = &[
    "auth_results",
    "sender",
    "links",
    "content",
    "attachments",
    "ledger",
];

/// The shipped local analyzers.
pub fn default_analyzers() -> Vec<Box<dyn Analyzer>> {
    vec![
        Box::new(Pure {
            name: "auth_results",
            run: auth_results::analyze,
        }),
        Box::new(Pure {
            name: "sender",
            run: sender::analyze,
        }),
        Box::new(Pure {
            name: "links",
            run: links::analyze,
        }),
        Box::new(Pure {
            name: "content",
            run: content::analyze,
        }),
        Box::new(Pure {
            name: "attachments",
            run: attachments::analyze,
        }),
        Box::new(ledger::LedgerAnalyzer),
    ]
}

/// Run every enabled analyzer and combine.
pub fn evaluate(
    input: &ThreatInput,
    analyzers: &[Box<dyn Analyzer>],
    config: &ThreatConfig,
) -> ThreatVerdict {
    let mut signals = Vec::new();
    let mut run = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = false;
    for analyzer in analyzers {
        let name = analyzer.name();
        if !config.analyzer_enabled(name) {
            skipped.push(SkippedAnalyzer {
                name: name.to_string(),
                reason: format!("disabled by threat.analyzers.{name}"),
            });
            continue;
        }
        match analyzer.analyze(input) {
            Ok(found) => {
                run.push(name.to_string());
                signals.extend(found);
            }
            Err(reason) => {
                if analyzer.required() {
                    failed = true;
                }
                skipped.push(SkippedAnalyzer {
                    name: name.to_string(),
                    reason: format!("error: {reason}"),
                });
            }
        }
    }
    combine(signals, run, skipped, failed)
}

/// Sum weights (cap 100) into a verdict. `failed` forces `unavailable`.
pub fn combine(
    signals: Vec<Signal>,
    analyzers_run: Vec<String>,
    analyzers_skipped: Vec<SkippedAnalyzer>,
    failed: bool,
) -> ThreatVerdict {
    let raw: u32 = signals.iter().map(|s| s.weight).sum();
    let score = raw.min(MAX_SCORE);
    let level = if failed {
        Level::Unavailable
    } else {
        Level::for_score(score)
    };
    ThreatVerdict {
        score,
        level,
        signals,
        analyzers_run,
        analyzers_skipped,
        engine_version: ENGINE_VERSION.to_string(),
        computed_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// The score arithmetic, one line per signal, for `threat explain` and the
/// reader's "Why?".
pub fn explain(verdict: &ThreatVerdict) -> Vec<String> {
    let mut lines = Vec::new();
    if verdict.signals.is_empty() {
        lines.push("no signals".to_string());
    }
    for s in &verdict.signals {
        let marker = if s.malware { " [malware]" } else { "" };
        lines.push(format!(
            "+{:>3}  {}{}  ({})",
            s.weight, s.code, marker, s.evidence
        ));
    }
    let raw: u32 = verdict.signals.iter().map(|s| s.weight).sum();
    if raw > MAX_SCORE {
        lines.push(format!("= {raw}, capped at {MAX_SCORE}"));
    } else {
        lines.push(format!("= {raw}"));
    }
    for skip in &verdict.analyzers_skipped {
        lines.push(format!("skipped {}: {}", skip.name, skip.reason));
    }
    let rule = match verdict.level {
        Level::Unavailable => "a required analyzer failed".to_string(),
        Level::Dangerous => format!("score >= {DANGEROUS_THRESHOLD}"),
        Level::Suspicious => format!("{SUSPICIOUS_THRESHOLD} <= score < {DANGEROUS_THRESHOLD}"),
        Level::Clean => format!("score < {SUSPICIOUS_THRESHOLD}"),
    };
    lines.push(format!("level {} ({rule})", verdict.level.as_str()));
    lines
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Build a message from header lines plus a body, with an empty ledger.
    pub fn input_from(headers: &[&str], body: &str) -> ThreatInput {
        let mut raw = headers.join("\r\n");
        raw.push_str("\r\n\r\n");
        raw.push_str(body);
        let mut input = ThreatInput::from_raw(raw.as_bytes(), "me@example.org").unwrap();
        input.ledger = Ok(CorrespondentFacts::default());
        input
    }

    pub fn codes(signals: &[Signal]) -> Vec<&str> {
        signals.iter().map(|s| s.code.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn sig(weight: u32) -> Signal {
        Signal::new("test", weight, "fixture")
    }

    #[test]
    fn combiner_thresholds_are_inclusive_at_30_and_70() {
        assert_eq!(
            combine(vec![sig(29)], vec![], vec![], false).level,
            Level::Clean
        );
        assert_eq!(
            combine(vec![sig(30)], vec![], vec![], false).level,
            Level::Suspicious
        );
        assert_eq!(
            combine(vec![sig(69)], vec![], vec![], false).level,
            Level::Suspicious
        );
        assert_eq!(
            combine(vec![sig(70)], vec![], vec![], false).level,
            Level::Dangerous
        );
    }

    #[test]
    fn combiner_caps_the_score_at_100() {
        let v = combine(vec![sig(60), sig(70)], vec![], vec![], false);
        assert_eq!(v.score, 100);
        assert_eq!(v.level, Level::Dangerous);
        assert!(explain(&v).iter().any(|l| l == "= 130, capped at 100"));
    }

    #[test]
    fn failed_required_analyzer_is_unavailable_even_with_no_signals() {
        let v = combine(vec![], vec!["sender".into()], vec![], true);
        assert_eq!(v.level, Level::Unavailable);
        assert_eq!(v.score, 0);
    }

    struct Broken {
        required: bool,
    }

    impl Analyzer for Broken {
        fn name(&self) -> &'static str {
            "broken"
        }
        fn required(&self) -> bool {
            self.required
        }
        fn analyze(&self, _: &ThreatInput) -> Result<Vec<Signal>, String> {
            Err("socket closed".to_string())
        }
    }

    #[test]
    fn required_analyzer_error_fails_closed_and_optional_one_is_skipped() {
        let input = input_from(&["From: a@b.example", "Subject: hi"], "hello");
        let config = ThreatConfig::default();

        let required: Vec<Box<dyn Analyzer>> = vec![Box::new(Broken { required: true })];
        let v = evaluate(&input, &required, &config);
        assert_eq!(v.level, Level::Unavailable);
        assert_eq!(v.analyzers_skipped[0].name, "broken");
        assert!(v.analyzers_skipped[0].reason.contains("socket closed"));

        let optional: Vec<Box<dyn Analyzer>> = vec![Box::new(Broken { required: false })];
        let v = evaluate(&input, &optional, &config);
        assert_eq!(v.level, Level::Clean);
        assert_eq!(v.analyzers_skipped.len(), 1);
    }

    #[test]
    fn unloaded_ledger_makes_the_verdict_unavailable() {
        let mut input = input_from(&["From: a@b.example"], "hello");
        input.ledger = Err("database locked".to_string());
        let v = evaluate(&input, &default_analyzers(), &ThreatConfig::default());
        assert_eq!(v.level, Level::Unavailable);
        assert!(v.analyzers_skipped.iter().any(|s| s.name == "ledger"));
    }

    #[test]
    fn disabled_analyzer_is_reported_as_skipped_not_failed() {
        let input = input_from(&["From: a@b.example"], "hello");
        let mut config = ThreatConfig::default();
        config.analyzers.insert("ledger".to_string(), false);
        let v = evaluate(&input, &default_analyzers(), &config);
        assert_ne!(v.level, Level::Unavailable);
        assert!(!v.analyzers_run.contains(&"ledger".to_string()));
        assert_eq!(
            v.analyzers_skipped[0].reason,
            "disabled by threat.analyzers.ledger"
        );
    }

    #[test]
    fn header_block_unfolds_continuations_in_wire_order() {
        let headers = parse_header_block(
            b"Received: from a\r\n\tby mx.example.org; Mon\r\nFrom: x@y\r\n\r\nBody: no\r\n",
        );
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].1, "from a by mx.example.org; Mon");
        assert_eq!(
            received_by_host(&headers[0].1).as_deref(),
            Some("mx.example.org")
        );
    }

    #[test]
    fn receiving_host_skips_opaque_by_ids() {
        let input = input_from(
            &[
                "Received: by 2002:a05:6a10:1234 with SMTP id x; Mon, 1 Jan 2026",
                "Received: from mail.sender.example by mx.google.com with ESMTPS id y",
                "From: a@sender.example",
            ],
            "x",
        );
        assert_eq!(input.receiving_host.as_deref(), Some("mx.google.com"));
    }
}
