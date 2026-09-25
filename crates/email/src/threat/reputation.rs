// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Domain reputation: Spamhaus DBL over DNS. Off unless
//! `threat.reputation.provider = spamhaus-dbl`.
//!
//! Only registrable domains leave the machine: the From domain and the link
//! domains, deduped and capped at [`MAX_DOMAINS`] per message. Never a full
//! URL, path, address or body. Every query that goes out is written as a
//! `lookup_performed` event (`{provider, domain, result}`); answers are
//! cached for [`CACHE_TTL_SECS`] under the app data directory so a domain is
//! asked about at most once an hour.
//!
//! Spamhaus answers `127.0.1.x` for a listed domain and NXDOMAIN for an
//! unlisted one. `127.255.255.x` means the query was refused (typo, public
//! resolver, rate limit), and any other answer means something between us
//! and Spamhaus rewrote it. None of those is "clean": the analyzer fails and,
//! because it is required once enabled, the verdict is `unavailable`.
//!
//! DNS goes through the resolver named in `/etc/resolv.conf` via
//! `hickory-resolver`, which the crate already depends on for account
//! discovery, so this adds no dependency. `std`'s `getaddrinfo` path was the
//! other candidate; it cannot tell NXDOMAIN (not listed) from SERVFAIL
//! (unknown), and that difference is the whole answer here.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::domains::{domain_of, registrable};
use super::{Analyzer, LookupLog, LookupRecord, Signal, ThreatInput, is_dns_name, links};

pub const PROVIDER: &str = "spamhaus-dbl";
/// Free public zone. Spamhaus refuses it through public resolvers.
pub const PUBLIC_ZONE: &str = "dbl.spamhaus.org";
/// Data Query Service zone, queried as `<domain>.<key>.dbl.dq.spamhaus.net`.
pub const DQS_ZONE: &str = "dbl.dq.spamhaus.net";
pub const MAX_DOMAINS: usize = 10;
pub const CACHE_TTL_SECS: i64 = 3600;
pub const CACHE_FILE_NAME: &str = "threat-reputation-cache.json";
/// Wall-clock limit for one DNS query, retries included.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

pub const PHISH_OR_MALWARE: u32 = 60;
pub const SPAM: u32 = 25;
pub const ABUSED_REDIRECTOR: u32 = 15;

/// What the resolver said about one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsAnswer {
    /// NXDOMAIN, or NOERROR with no A records.
    NoRecords,
    A(Vec<Ipv4Addr>),
}

/// The DNS seam. Errors must not echo the query name: with DQS it carries
/// the key.
pub trait DnsResolver: Send + Sync {
    fn lookup_a(&self, fqdn: &str) -> Result<DnsAnswer, String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DblResult {
    NotListed,
    Listed { category: String, code: u8 },
}

impl DblResult {
    /// The `result` field of a `lookup_performed` event.
    pub fn label(&self) -> String {
        match self {
            DblResult::NotListed => "not_listed".to_string(),
            DblResult::Listed { category, .. } => format!("listed:{category}"),
        }
    }
}

/// Category and weight for a DBL `127.0.1.<code>` answer.
pub fn category(code: u8) -> (&'static str, u32) {
    match code {
        2 => ("spam", SPAM),
        4 => ("phish", PHISH_OR_MALWARE),
        5 => ("malware", PHISH_OR_MALWARE),
        6 => ("botnet_cc", PHISH_OR_MALWARE),
        102 => ("abused_legit_spam", SPAM),
        103 => ("abused_redirector", ABUSED_REDIRECTOR),
        104 => ("abused_legit_phish", PHISH_OR_MALWARE),
        105 => ("abused_legit_malware", PHISH_OR_MALWARE),
        106 => ("abused_legit_botnet_cc", PHISH_OR_MALWARE),
        // A listing code newer than this table: listed, weighed as spam.
        _ => ("listed_unknown", SPAM),
    }
}

/// Turn a DNS answer into a DBL result. `Err` for every refusal and every
/// answer outside `127.0.1.0/24`.
pub fn classify(answer: &DnsAnswer) -> Result<DblResult, String> {
    let ips = match answer {
        DnsAnswer::NoRecords => return Ok(DblResult::NotListed),
        DnsAnswer::A(ips) if ips.is_empty() => return Ok(DblResult::NotListed),
        DnsAnswer::A(ips) => ips,
    };
    if let Some(err) = ips.iter().find(|ip| ip.octets()[..3] == [127, 255, 255]) {
        let why = match err.octets()[3] {
            252 => "typo in the DNSBL name",
            254 => "query came through a public or open resolver",
            255 => "too many queries",
            _ => "query refused",
        };
        return Err(format!("Spamhaus refused the query ({err}: {why})"));
    }
    let mut best: Option<(u8, u32)> = None;
    for ip in ips {
        let [a, b, c, code] = ip.octets();
        if [a, b, c] != [127, 0, 1] {
            return Err(format!(
                "unexpected DBL answer {ip}; a resolver may be rewriting DNS"
            ));
        }
        if code == 255 {
            return Err("Spamhaus refused the query (127.0.1.255: IP queries prohibited)".into());
        }
        let weight = category(code).1;
        if best.is_none_or(|(_, w)| weight > w) {
            best = Some((code, weight));
        }
    }
    let (code, _) = best.expect("non-empty answer");
    Ok(DblResult::Listed {
        category: category(code).0.to_string(),
        code,
    })
}

/// The name to query for `domain`, fully qualified.
pub fn query_name(domain: &str, dqs_key: Option<&str>) -> String {
    match dqs_key {
        Some(key) => format!("{domain}.{key}.{DQS_ZONE}."),
        None => format!("{domain}.{PUBLIC_ZONE}."),
    }
}

/// The registrable domains a message would disclose: From first, then link
/// hosts in order, deduped, IP literals dropped, capped at [`MAX_DOMAINS`].
pub fn candidate_domains(input: &ThreatInput) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let from = domain_of(&input.from_addr).into_iter();
    for host in from.chain(links::link_hosts(input)) {
        if out.len() >= MAX_DOMAINS {
            break;
        }
        if !is_dns_name(&host) {
            continue;
        }
        let domain = registrable(&host);
        if !domain.is_empty() && !out.contains(&domain) {
            out.push(domain);
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CacheEntry {
    result: DblResult,
    expires_at: i64,
}

/// The on-disk cache: `{"<provider>:<domain>": {result, expires_at}}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheContents {
    entries: BTreeMap<String, CacheEntry>,
}

impl CacheContents {
    pub fn fresh(&self, domain: &str, now: i64) -> Option<DblResult> {
        self.entries
            .get(&format!("{PROVIDER}:{domain}"))
            .filter(|e| e.expires_at > now)
            .map(|e| e.result.clone())
    }

    pub fn insert(&mut self, domain: &str, result: DblResult, now: i64) {
        self.entries.insert(
            format!("{PROVIDER}:{domain}"),
            CacheEntry {
                result,
                expires_at: now + CACHE_TTL_SECS,
            },
        );
    }
}

/// TTL cache file. Only definitive answers are cached; a refusal is asked
/// again next time.
#[derive(Debug, Clone)]
pub struct ReputationCache {
    path: PathBuf,
}

impl ReputationCache {
    pub fn new(path: PathBuf) -> Self {
        ReputationCache { path }
    }

    pub fn default_path() -> PathBuf {
        envelope_email_store::app_data_dir().join(CACHE_FILE_NAME)
    }

    pub fn load(&self) -> Result<CacheContents, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(CacheContents::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                format!(
                    "reputation cache {} is unreadable ({e}); delete it to rebuild",
                    self.path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CacheContents::default()),
            Err(e) => Err(format!(
                "reputation cache {} is unreadable: {e}",
                self.path.display()
            )),
        }
    }

    /// Write atomically, dropping expired entries.
    pub fn save(&self, contents: &CacheContents, now: i64) -> Result<(), String> {
        let mut kept = contents.clone();
        kept.entries.retain(|_, e| e.expires_at > now);
        let text = serde_json::to_string(&kept).map_err(|e| e.to_string())?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let tmp = self
            .path
            .with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("replace {}: {e}", self.path.display())
        })
    }
}

pub struct ReputationAnalyzer {
    resolver: Box<dyn DnsResolver>,
    dqs_key: Option<String>,
    cache: ReputationCache,
    log: LookupLog,
    now: fn() -> i64,
}

impl ReputationAnalyzer {
    pub fn new(
        resolver: Box<dyn DnsResolver>,
        dqs_key: Option<String>,
        cache: ReputationCache,
        log: LookupLog,
    ) -> Self {
        ReputationAnalyzer {
            resolver,
            dqs_key,
            cache,
            log,
            now: || chrono::Utc::now().timestamp(),
        }
    }

    fn record(&self, domain: &str, result: String) {
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(LookupRecord::new(PROVIDER, domain, result));
    }

    fn scrub(&self, message: String) -> String {
        match &self.dqs_key {
            Some(key) => message.replace(key.as_str(), "<dqs-key>"),
            None => message,
        }
    }
}

impl Analyzer for ReputationAnalyzer {
    fn name(&self) -> &'static str {
        "reputation"
    }

    fn analyze(&self, input: &ThreatInput) -> Result<Vec<Signal>, String> {
        let domains = candidate_domains(input);
        if domains.is_empty() {
            return Ok(Vec::new());
        }
        let now = (self.now)();
        let mut cache = self.cache.load()?;
        let mut dirty = false;
        let mut signals = Vec::new();
        let mut failure = None;
        for domain in &domains {
            let result = match cache.fresh(domain, now) {
                Some(hit) => hit,
                None => {
                    let asked = self
                        .resolver
                        .lookup_a(&query_name(domain, self.dqs_key.as_deref()))
                        .and_then(|answer| classify(&answer))
                        .map_err(|e| self.scrub(e));
                    match asked {
                        Ok(result) => {
                            self.record(domain, result.label());
                            cache.insert(domain, result.clone(), now);
                            dirty = true;
                            result
                        }
                        Err(e) => {
                            self.record(domain, format!("unavailable: {e}"));
                            // A refusal will repeat for every domain; stop asking.
                            failure = Some(format!("{PROVIDER} lookup for {domain}: {e}"));
                            break;
                        }
                    }
                }
            };
            if let DblResult::Listed { category, code } = &result {
                signals.push(Signal::new(
                    "domain_blocklisted",
                    self::category(*code).1,
                    format!("{domain} listed by Spamhaus DBL as {category} (127.0.1.{code})"),
                ));
            }
        }
        if dirty {
            self.cache.save(&cache, now)?;
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(signals),
        }
    }
}

/// [`DnsResolver`] over the system's configured nameservers. Each query runs
/// on its own thread and runtime, so the analyzer can be called from sync
/// code inside or outside a Tokio runtime.
pub struct SystemDns;

impl DnsResolver for SystemDns {
    fn lookup_a(&self, fqdn: &str) -> Result<DnsAnswer, String> {
        let name = fqdn.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("envelope-dnsbl".into())
            .spawn(move || {
                let _ = tx.send(system_lookup(&name));
            })
            .map_err(|e| format!("could not start the DNS thread: {e}"))?;
        rx.recv_timeout(LOOKUP_TIMEOUT)
            .map_err(|_| format!("DNS lookup timed out after {LOOKUP_TIMEOUT:?}"))?
    }
}

fn system_lookup(name: &str) -> Result<DnsAnswer, String> {
    use hickory_resolver::TokioAsyncResolver;
    use hickory_resolver::error::ResolveErrorKind;
    use hickory_resolver::proto::op::ResponseCode;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start the DNS runtime: {e}"))?;
    runtime.block_on(async {
        let (config, mut opts) = hickory_resolver::system_conf::read_system_conf()
            .map_err(|e| format!("system DNS configuration is unreadable: {e}"))?;
        opts.timeout = Duration::from_secs(2);
        opts.attempts = 2;
        let resolver = TokioAsyncResolver::tokio(config, opts);
        match resolver.ipv4_lookup(name).await {
            Ok(lookup) => Ok(DnsAnswer::A(lookup.iter().map(|a| a.0).collect())),
            Err(e) => match e.kind() {
                ResolveErrorKind::NoRecordsFound { response_code, .. }
                    if matches!(
                        *response_code,
                        ResponseCode::NXDomain | ResponseCode::NoError
                    ) =>
                {
                    Ok(DnsAnswer::NoRecords)
                }
                ResolveErrorKind::NoRecordsFound { response_code, .. } => {
                    Err(format!("DNS server answered {response_code}"))
                }
                ResolveErrorKind::Timeout => Err("DNS lookup timed out".into()),
                ResolveErrorKind::NoConnections => Err("no DNS server is reachable".into()),
                _ => Err("DNS lookup failed".into()),
            },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::input_from;
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Answers from a table; counts queries; never touches the network.
    struct FakeDns {
        answers: Vec<(&'static str, Result<DnsAnswer, String>)>,
        asked: Arc<Mutex<Vec<String>>>,
    }

    impl DnsResolver for FakeDns {
        fn lookup_a(&self, fqdn: &str) -> Result<DnsAnswer, String> {
            self.asked.lock().unwrap().push(fqdn.to_string());
            self.answers
                .iter()
                .find(|(name, _)| fqdn.starts_with(name))
                .map(|(_, a)| a.clone())
                .unwrap_or(Ok(DnsAnswer::NoRecords))
        }
    }

    fn a(ip: [u8; 4]) -> Result<DnsAnswer, String> {
        Ok(DnsAnswer::A(vec![Ipv4Addr::from(ip)]))
    }

    fn analyzer(
        answers: Vec<(&'static str, Result<DnsAnswer, String>)>,
        dir: &tempfile::TempDir,
        dqs_key: Option<&str>,
    ) -> (ReputationAnalyzer, LookupLog, Arc<Mutex<Vec<String>>>) {
        let log = LookupLog::default();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let a = ReputationAnalyzer::new(
            Box::new(FakeDns {
                answers,
                asked: asked.clone(),
            }),
            dqs_key.map(str::to_string),
            ReputationCache::new(dir.path().join(CACHE_FILE_NAME)),
            log.clone(),
        );
        (a, log, asked)
    }

    fn phish_input() -> ThreatInput {
        input_from(
            &[
                "From: Support <help@mail.dbltest.com>",
                "MIME-Version: 1.0",
                "Content-Type: text/html; charset=utf-8",
            ],
            r#"<a href="https://login.bad-phish.example/reset?token=SECRET123&u=me%40example.org">Reset</a>
               <a href="https://www.dbltest.com/other/path">dup</a>
               <a href="http://203.0.113.9/x">ip</a>"#,
        )
    }

    #[test]
    fn dbl_answers_classify_listed_unlisted_and_refused() {
        assert_eq!(classify(&DnsAnswer::NoRecords), Ok(DblResult::NotListed));
        assert_eq!(
            classify(&a([127, 0, 1, 2]).unwrap()),
            Ok(DblResult::Listed {
                category: "spam".into(),
                code: 2
            })
        );
        assert_eq!(category(4), ("phish", 60));
        assert_eq!(category(5), ("malware", 60));
        assert_eq!(category(103), ("abused_redirector", 15));
        assert_eq!(category(102).1, 25);
        // The heaviest category wins when several codes come back.
        let both = DnsAnswer::A(vec![
            Ipv4Addr::new(127, 0, 1, 2),
            Ipv4Addr::new(127, 0, 1, 4),
        ]);
        assert_eq!(
            classify(&both).unwrap(),
            DblResult::Listed {
                category: "phish".into(),
                code: 4
            }
        );
        for last in [252, 254, 255, 1] {
            let err = classify(&a([127, 255, 255, last]).unwrap()).unwrap_err();
            assert!(err.contains("refused"), "{last}: {err}");
        }
        assert!(classify(&a([127, 0, 1, 255]).unwrap()).is_err());
        // An NXDOMAIN-hijacking resolver answering with an ad server.
        assert!(
            classify(&a([198, 51, 100, 7]).unwrap())
                .unwrap_err()
                .contains("unexpected")
        );
    }

    #[test]
    fn query_names_use_the_public_zone_or_dqs() {
        assert_eq!(query_name("x.com", None), "x.com.dbl.spamhaus.org.");
        assert_eq!(
            query_name("x.com", Some("k3y")),
            "x.com.k3y.dbl.dq.spamhaus.net."
        );
    }

    #[test]
    fn candidates_are_registrable_deduped_and_skip_ip_literals() {
        assert_eq!(
            candidate_domains(&phish_input()),
            vec!["dbltest.com".to_string(), "bad-phish.example".to_string()]
        );
    }

    #[test]
    fn listed_from_domain_signals_and_every_query_is_logged_domain_only() {
        let dir = tempfile::tempdir().unwrap();
        let (analyzer, log, asked) = analyzer(
            vec![
                ("dbltest.com.", a([127, 0, 1, 2])),
                ("bad-phish.example.", a([127, 0, 1, 4])),
            ],
            &dir,
            None,
        );
        let signals = analyzer.analyze(&phish_input()).unwrap();
        assert_eq!(signals.len(), 2);
        assert!(signals.iter().all(|s| s.code == "domain_blocklisted"));
        assert_eq!(signals[0].weight, 25);
        assert_eq!(signals[1].weight, 60);
        assert!(signals[1].evidence.starts_with("bad-phish.example listed"));

        assert_eq!(
            *asked.lock().unwrap(),
            vec![
                "dbltest.com.dbl.spamhaus.org.",
                "bad-phish.example.dbl.spamhaus.org."
            ]
        );
        let records = log.lock().unwrap().clone();
        assert_eq!(
            records,
            vec![
                LookupRecord::new(PROVIDER, "dbltest.com", "listed:spam".to_string()),
                LookupRecord::new(PROVIDER, "bad-phish.example", "listed:phish".to_string()),
            ]
        );
        let serialized = serde_json::to_string(&records).unwrap();
        for leaked in ["http", "reset", "SECRET123", "login.", "me%40", "/"] {
            assert!(!serialized.contains(leaked), "{leaked} in {serialized}");
        }
        for s in &signals {
            assert!(!s.evidence.contains("SECRET123") && !s.evidence.contains("http"));
        }
    }

    #[test]
    fn public_resolver_refusal_fails_the_analyzer_and_stops_asking() {
        let dir = tempfile::tempdir().unwrap();
        let (analyzer, log, asked) =
            analyzer(vec![("dbltest.com.", a([127, 255, 255, 254]))], &dir, None);
        let err = analyzer.analyze(&phish_input()).unwrap_err();
        assert!(err.contains("public or open resolver"), "{err}");
        assert_eq!(asked.lock().unwrap().len(), 1, "no second query");
        let records = log.lock().unwrap().clone();
        assert_eq!(records.len(), 1);
        assert!(records[0].result.starts_with("unavailable:"));
        // Refusals are not cached.
        let cache = ReputationCache::new(dir.path().join(CACHE_FILE_NAME));
        assert_eq!(cache.load().unwrap().fresh("dbltest.com", 0), None);
    }

    #[test]
    fn resolver_errors_never_leak_the_dqs_key() {
        let dir = tempfile::tempdir().unwrap();
        let (analyzer, log, _) = analyzer(
            vec![(
                "dbltest.com.",
                Err("SERVFAIL for dbltest.com.s3cretkey.dbl.dq.spamhaus.net.".into()),
            )],
            &dir,
            Some("s3cretkey"),
        );
        let err = analyzer.analyze(&phish_input()).unwrap_err();
        assert!(!err.contains("s3cretkey"), "{err}");
        let serialized = serde_json::to_string(&*log.lock().unwrap()).unwrap();
        assert!(!serialized.contains("s3cretkey"));
    }

    #[test]
    fn cache_serves_fresh_answers_and_expires_after_the_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ReputationCache::new(dir.path().join("nested").join(CACHE_FILE_NAME));
        let mut contents = cache.load().unwrap();
        contents.insert("dbltest.com", DblResult::NotListed, 1_000);
        cache.save(&contents, 1_000).unwrap();

        let loaded = cache.load().unwrap();
        assert_eq!(
            loaded.fresh("dbltest.com", 1_000 + CACHE_TTL_SECS - 1),
            Some(DblResult::NotListed)
        );
        assert_eq!(loaded.fresh("dbltest.com", 1_000 + CACHE_TTL_SECS), None);
        // Saving after expiry prunes the entry.
        cache.save(&loaded, 1_000 + CACHE_TTL_SECS).unwrap();
        assert_eq!(cache.load().unwrap(), CacheContents::default());
    }

    #[test]
    fn second_scan_within_the_ttl_asks_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (analyzer, log, asked) =
            analyzer(vec![("dbltest.com.", a([127, 0, 1, 2]))], &dir, None);
        let first = analyzer.analyze(&phish_input()).unwrap();
        let second = analyzer.analyze(&phish_input()).unwrap();
        assert_eq!(first, second, "cache hit gives the same signal");
        assert_eq!(
            asked.lock().unwrap().len(),
            2,
            "two domains, asked once each"
        );
        assert_eq!(log.lock().unwrap().len(), 2, "cache hits are not lookups");
    }

    #[test]
    fn corrupt_cache_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CACHE_FILE_NAME), "{not json").unwrap();
        let (analyzer, _, asked) = analyzer(vec![], &dir, None);
        let err = analyzer.analyze(&phish_input()).unwrap_err();
        assert!(err.contains("delete it to rebuild"), "{err}");
        assert!(asked.lock().unwrap().is_empty());
    }
}
