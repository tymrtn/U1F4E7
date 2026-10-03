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
//! Analyzers plug in through [`Analyzer`]. The local six always run; clamd
//! ([`clamd`]) and domain reputation ([`reputation`]) are opt-in, may do I/O,
//! and declare whether their failure is fatal through [`Analyzer::required`].
//! [`configured_analyzers`] assembles the set a config asks for.
//!
//! Signal evidence carries hosts, domains, extensions and hashes only — never
//! bodies, subjects or full URLs — because verdicts are stored in the
//! `threat_verdict` event payload.

pub mod attachments;
pub mod auth_results;
pub mod clamd;
pub mod config;
pub mod content;
pub mod domains;
pub mod ledger;
pub mod links;
pub mod persist;
pub mod rdap;
pub mod report;
pub mod reputation;
pub mod sender;

use mail_parser::MimeHeaders;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use config::{Quarantine, ReputationProvider, ThreatConfig};
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

/// Header fields a content fingerprint covers besides every `Content-*` field:
/// the ones the sender writes. Fields a receiving server adds (`Received`,
/// `Authentication-Results`, `Delivered-To`, spam scores) are left out, so a
/// message keeps its fingerprint in every folder it is delivered or moved to.
const FINGERPRINT_HEADERS: &[&str] = &[
    "from",
    "sender",
    "reply-to",
    "to",
    "cc",
    "subject",
    "date",
    "message-id",
    "mime-version",
    "list-unsubscribe",
    "list-unsubscribe-post",
];

/// The identity of a message's content: `v1:` and the SHA-256 of its
/// fingerprinted header fields (every occurrence, in wire order, as sent) and
/// its raw body after the first blank line. Stored verdicts and Mark safe
/// apply only to a message with the same fingerprint.
pub fn content_fingerprint(raw: &[u8]) -> String {
    let (fields, body) = raw_header_fields(raw);
    let mut hasher = Sha256::new();
    // Length-prefixed, so two different messages never hash the same bytes.
    let mut part = |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    };
    for field in fields.into_iter().filter(|f| fingerprinted(f)) {
        part(field);
    }
    part(body);
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("v1:{hex}")
}

/// `Content-*` fields change how the body is read, a top-level
/// `Content-Disposition` included, so all of them are fingerprinted.
fn fingerprinted(field: &[u8]) -> bool {
    let Some(colon) = field.iter().position(|&b| b == b':') else {
        return false;
    };
    let name = field[..colon].trim_ascii().to_ascii_lowercase();
    name.starts_with(b"content-") || FINGERPRINT_HEADERS.iter().any(|h| h.as_bytes() == name)
}

/// Each header field's bytes as sent (first line plus folded continuations,
/// without the final line break), and the body after the first blank line.
/// Lines split as in [`parse_header_block`].
fn raw_header_fields(raw: &[u8]) -> (Vec<&[u8]>, &[u8]) {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0;
    while pos < raw.len() {
        let end = raw[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(raw.len(), |i| pos + i);
        let line_end = if end > pos && raw[end - 1] == b'\r' {
            end - 1
        } else {
            end
        };
        let next = (end + 1).min(raw.len());
        if line_end == pos {
            return (spans_of(raw, &spans), &raw[next..]);
        }
        if matches!(raw[pos], b' ' | b'\t') {
            if let Some(last) = spans.last_mut() {
                last.1 = line_end;
            }
        } else {
            spans.push((pos, line_end));
        }
        pos = next;
    }
    (spans_of(raw, &spans), &[])
}

fn spans_of<'a>(raw: &'a [u8], spans: &[(usize, usize)]) -> Vec<&'a [u8]> {
    spans.iter().map(|&(start, end)| &raw[start..end]).collect()
}

/// Threat data for a message without one usable Message-ID is keyed by this
/// prefix and the message's content fingerprint.
pub const FINGERPRINT_KEY_PREFIX: &str = "fp:";

/// The message's identity as every threat path reads it (scanner, attachment
/// gate, quarantine, views): the canonical Message-ID when the header block
/// has exactly one Message-ID field holding one non-empty id. `None` when it
/// has none, several, or an empty or malformed one; that message's threat
/// data is keyed by its fingerprint instead.
pub fn sole_message_id(raw: &[u8]) -> Option<String> {
    sole_message_id_in(&parse_header_block(raw))
}

/// [`sole_message_id`] over already parsed header fields.
pub(crate) fn sole_message_id_in(headers: &[(String, String)]) -> Option<String> {
    let [id] = message_id_values_in(headers).try_into().ok()?;
    let one_id =
        !id.is_empty() && !id.contains(|c: char| c.is_whitespace() || c == '<' || c == '>');
    one_id.then_some(id)
}

/// The canonical value of every Message-ID field, in wire order, empty ones
/// included: what a server may report as the message's Message-ID.
pub(crate) fn message_id_values_in(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("message-id"))
        .map(|(_, value)| envelope_email_store::canonical_message_id(value).to_string())
        .collect()
}

/// [`message_id_values_in`] of a raw message.
pub fn message_id_values(raw: &[u8]) -> Vec<String> {
    message_id_values_in(&parse_header_block(raw))
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

/// One question an analyzer or `threat report` asked an outside service,
/// stored as a `lookup_performed` event. Domain only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupRecord {
    pub provider: String,
    pub domain: String,
    pub result: String,
}

impl LookupRecord {
    pub fn new(provider: &str, domain: &str, result: impl Into<String>) -> Self {
        LookupRecord {
            provider: provider.to_string(),
            domain: domain.to_string(),
            result: result.into(),
        }
    }
}

/// Where I/O analyzers leave their [`LookupRecord`]s for the caller to store.
pub type LookupLog = std::sync::Arc<std::sync::Mutex<Vec<LookupRecord>>>;

/// The local analyzers plus whichever opt-in analyzers `config` enables.
/// Reputation lookups land in `log`.
pub fn configured_analyzers(
    config: &ThreatConfig,
    log: &LookupLog,
) -> Result<Vec<Box<dyn Analyzer>>, String> {
    let mut analyzers = default_analyzers();
    if let Some(address) = &config.clamd {
        analyzers.push(Box::new(clamd::ClamdAnalyzer {
            address: address.clone(),
            required: config.clamd_required,
        }));
    }
    match config.reputation_provider {
        ReputationProvider::Off => {}
        ReputationProvider::SpamhausDbl => {
            let key = config.dqs_key().map_err(|e| format!("{e:#}"))?;
            analyzers.push(Box::new(reputation::ReputationAnalyzer::new(
                Box::new(reputation::SystemDns),
                key,
                reputation::ReputationCache::new(reputation::ReputationCache::default_path()),
                log.clone(),
            )));
        }
    }
    Ok(analyzers)
}

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
    fn opt_in_analyzers_join_only_when_configured() {
        let log = LookupLog::default();
        let names = |c: &ThreatConfig| -> Vec<&'static str> {
            configured_analyzers(c, &log)
                .unwrap()
                .iter()
                .map(|a| a.name())
                .collect()
        };
        let mut config = ThreatConfig::default();
        assert_eq!(names(&config), ANALYZER_NAMES.to_vec());

        config.clamd = Some(config::ClamdAddress::Tcp("127.0.0.1:3310".into()));
        config.reputation_provider = ReputationProvider::SpamhausDbl;
        config.dqs_key = Some("k3y".into());
        let with = names(&config);
        assert_eq!(&with[ANALYZER_NAMES.len()..], ["clamd", "reputation"]);
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

    const LUNCH: &str = "From: Alice <alice@partner.example>\r\n\
                         To: me@example.org\r\n\
                         Subject: Lunch\r\n\
                         Date: Mon, 21 Sep 2026 10:00:00 +0000\r\n\
                         Message-ID: <m1@partner.example>\r\n\
                         MIME-Version: 1.0\r\n\
                         Content-Type: multipart/mixed; boundary=b\r\n\
                         \r\n\
                         --b\r\nContent-Type: text/plain\r\n\r\nThursday?\r\n--b--\r\n";

    #[test]
    fn fingerprint_ignores_receiver_headers() {
        let fp = content_fingerprint(LUNCH.as_bytes());
        assert!(fp.starts_with("v1:"), "{fp}");
        assert_eq!(fp.len(), 3 + 64, "{fp}");

        let delivered = format!(
            "Return-Path: <alice@partner.example>\r\n\
             Delivered-To: me@example.org\r\n\
             Received: from mail.partner.example by mx1.example.org with ESMTPS; \
             Mon, 21 Sep 2026 10:00:01 +0000\r\n\
             Authentication-Results: mx1.example.org; spf=pass; dkim=pass; dmarc=pass\r\n\
             X-Spam-Status: No, score=-0.1\r\n{LUNCH}"
        );
        assert_eq!(content_fingerprint(delivered.as_bytes()), fp);
        let interleaved =
            LUNCH.replacen("Subject:", "X-Original-To: me@example.org\r\nSubject:", 1);
        assert_eq!(content_fingerprint(interleaved.as_bytes()), fp);
    }

    #[test]
    fn fingerprint_changes_with_body_attachment_or_second_from() {
        let fp = content_fingerprint(LUNCH.as_bytes());
        let body = LUNCH.replace("Thursday?", "Friday?");
        let attachment = LUNCH.replace(
            "--b--\r\n",
            "--b\r\nContent-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=\"invoice.pdf.exe\"\r\n\r\nMZ\r\n--b--\r\n",
        );
        let second_from = LUNCH.replacen("To:", "From: IT Desk <it@examp1e.org>\r\nTo:", 1);
        let subject = LUNCH.replace("Subject: Lunch", "Subject: Lunch!");
        let disposition = LUNCH.replacen(
            "MIME-Version: 1.0\r\n",
            "MIME-Version: 1.0\r\nContent-Disposition: attachment; filename=\"a.exe\"\r\n",
            1,
        );
        for changed in [body, attachment, second_from, subject, disposition] {
            assert_ne!(content_fingerprint(changed.as_bytes()), fp, "{changed}");
        }
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

    #[test]
    fn sole_message_id_needs_exactly_one_non_empty_id() {
        let cases: &[(&str, Option<&str>)] = &[
            ("Message-ID: <abc@host>\r\n", Some("abc@host")),
            ("message-id: <a@x>\r\n", Some("a@x")),
            ("Message-ID: a@x\r\n", Some("a@x")),
            ("Message-ID:\r\n <a@x>\r\n", Some("a@x")),
            ("Message-ID: < a@x >\r\n", Some("a@x")),
            ("", None),
            ("Message-ID: \r\n", None),
            ("Message-ID: <>\r\n", None),
            ("Message-ID: <first@x>\r\nMessage-ID: <second@x>\r\n", None),
            ("Message-ID: \r\nMessage-ID: <b@x>\r\n", None),
            ("Message-ID: <a@x> <b@x>\r\n", None),
            ("Message-ID: (c) <a@x> (trailing)\r\n", None),
        ];
        for (header, expected) in cases {
            let raw = format!("From: a@x\r\nTo: me@y\r\nSubject: s\r\n{header}\r\nbody\r\n");
            assert_eq!(
                sole_message_id(raw.as_bytes()).as_deref(),
                *expected,
                "{header:?}"
            );
        }
    }

    /// What mail_parser would show of a message.
    fn parsed_view(raw: &[u8]) -> String {
        let Some(m) = mail_parser::MessageParser::default().parse(raw) else {
            return "unparseable".into();
        };
        format!(
            "from={:?} subject={:?} ct={:?} text={:?} html={:?} attachments={:?}",
            m.from().and_then(|f| f.first()).and_then(|a| a.address()),
            m.subject(),
            m.content_type()
                .map(|c| format!("{}/{:?}", c.ctype(), c.subtype())),
            m.body_text(0),
            m.body_html(0),
            m.attachments()
                .map(|a| a.attachment_name().unwrap_or("?").to_string())
                .collect::<Vec<_>>(),
        )
    }

    /// Two messages that read differently never share a fingerprint, across
    /// header-block and line-ending edge cases.
    #[test]
    fn fingerprint_differs_whenever_the_parsed_message_differs() {
        const HDR: &str = "From: Alice <alice@partner.example>\r\nTo: me@example.org\r\n\
                           Subject: Lunch\r\nMessage-ID: <m1@partner.example>\r\n";
        let base = format!("{HDR}X-Pad: 1\r\n\r\nThursday?\r\n");
        let html = format!("{HDR}X-Pad: 1\r\n\r\n<b>Thursday?</b>\r\n");
        let with = |extra: &str| format!("{HDR}{extra}\r\nX-Pad: 1\r\n\r\nThursday?\r\n");
        let html_with =
            |extra: &str| format!("{HDR}{extra}\r\nX-Pad: 1\r\n\r\n<b>Thursday?</b>\r\n");
        let mut pairs: Vec<(String, String)> = vec![
            (
                base.clone(),
                format!(
                    "{HDR}X-Pad: 1\r\n \r\nPay at http://evil.example/login\r\n\r\nThursday?\r\n"
                ),
            ),
            (
                base.clone(),
                format!(
                    "{HDR}X-Pad: 1\r\n\t\r\nPay at http://evil.example/login\r\n\r\nThursday?\r\n"
                ),
            ),
            (
                base.clone(),
                format!("{HDR}X-Pad: 1\r\nPay now at evil.example\r\n\r\nThursday?\r\n"),
            ),
            (
                base.clone(),
                format!("{HDR}X-Pad: 1\r\r\nPay now at evil.example\r\n\r\nThursday?\r\n"),
            ),
            (
                base.clone(),
                format!("{HDR}X-Pad: 1\rContent-Type: text/html\r\n\r\nThursday?\r\n"),
            ),
            (
                base.clone(),
                format!("{HDR}X-Pad: 1\r\rPay now at evil.example\r\n\r\nThursday?\r\n"),
            ),
            (
                html.clone(),
                format!("{HDR}X-Pad: 1\r\n Content-Type: text/html\r\n\r\n<b>Thursday?</b>\r\n"),
            ),
            (base.clone(), base.replace("\r\n", "\n")),
            (
                format!(
                    "{HDR}Content-Type: text/plain\r\nContent-Type: text/html\r\n\r\n<b>x</b>\r\n"
                ),
                format!(
                    "{HDR}Content-Type: text/html\r\nContent-Type: text/plain\r\n\r\n<b>x</b>\r\n"
                ),
            ),
            (base.clone(), format!(" junk\r\n{base}")),
            (
                format!("{HDR}\r\n<b>x</b>\r\n"),
                format!("{HDR}Content-Type : text/html\r\n\r\n<b>x</b>\r\n"),
            ),
            (
                base.clone(),
                format!("{HDR}X-Pad: 1\r\n\0\r\nPay now\r\n\r\nThursday?\r\n"),
            ),
            (
                base.clone(),
                format!(
                    "{HDR}X-Pad: 1\r\n<html><a href=\"http://evil.example\">Pay</a></html>\r\n\r\nThursday?\r\n"
                ),
            ),
        ];
        for extra in [
            "Resent-From: Bank <security@bank.example>",
            "X-Original-From: ceo@example.org",
            "In-Reply-To: <thread@example.org>",
            "References: <thread@example.org>",
            "Return-Path: <x@evil.example>",
            "Disposition-Notification-To: x@evil.example",
            "Comments: Pay at http://evil.example",
            "Keywords: urgent",
            "Importance: high",
        ] {
            pairs.push((base.clone(), with(extra)));
        }
        for extra in [
            "From\x0b: Bank <sec@bank.example>",
            "Subject\x0b: URGENT pay",
            "\u{feff}Content-Type: text/html",
            "From\u{a0}: Bank <sec@bank.example>",
            "\x0cContent-Type: text/html",
            "X-Content-Type: text/html",
            "Content\x0b-Type: text/html",
            "Content_Type: text/html",
        ] {
            pairs.push((html.clone(), html_with(extra)));
        }
        for (a, b) in &pairs {
            let same_fingerprint =
                content_fingerprint(a.as_bytes()) == content_fingerprint(b.as_bytes());
            let same_view = parsed_view(a.as_bytes()) == parsed_view(b.as_bytes());
            assert!(!same_fingerprint || same_view, "{b:?}");
        }
    }
}
