// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Live Spamhaus DBL checks. Ignored by default because they need DNS.
//!
//! ```bash
//! export ENVELOPE_HOME=$(mktemp -d)
//! envelope config set threat.reputation.provider spamhaus-dbl
//! cargo test -p envelope-email-transport --test threat_reputation_live -- --ignored --nocapture
//! ```
//!
//! `dbltest.com` is Spamhaus's permanent DBL test entry (`127.0.1.2`).

use std::time::Duration;

use envelope_email_store::Database;
use envelope_email_transport::threat::persist;
use envelope_email_transport::threat::reputation::{
    CACHE_FILE_NAME, DnsAnswer, DnsResolver, ReputationAnalyzer, ReputationCache,
};
use envelope_email_transport::threat::{
    Analyzer, Level, LookupLog, ReputationProvider, ThreatConfig, ThreatInput, evaluate,
};

const FIXTURE: &[u8] = b"Message-ID: <live@x>\r\nFrom: Test <probe@dbltest.com>\r\n\
To: me@example.org\r\nSubject: DBL live check\r\n\r\nhello\r\n";

#[test]
#[ignore = "live DNS: queries Spamhaus through the system resolver"]
fn configured_spamhaus_dbl_flags_dbltest_com() {
    let home = std::env::var("ENVELOPE_HOME").expect("set ENVELOPE_HOME to a scratch dir");
    let config = ThreatConfig::load().expect("config.json readable");
    assert_eq!(
        config.reputation_provider,
        ReputationProvider::SpamhausDbl,
        "run `envelope config set threat.reputation.provider spamhaus-dbl` in {home}"
    );

    let db = Database::open_memory().unwrap();
    let (verdict, scanned) = persist::scan_raw(&db, "acct", "me@example.org", FIXTURE, &config);
    for line in envelope_email_transport::threat::explain(&verdict) {
        println!("{line}");
    }
    println!("lookups: {:?}", scanned.lookups);
    assert!(
        verdict
            .signals
            .iter()
            .any(|s| s.code == "domain_blocklisted" && s.evidence.starts_with("dbltest.com")),
        "{verdict:?}"
    );
    assert!(verdict.analyzers_run.contains(&"reputation".to_string()));
    assert_eq!(scanned.lookups[0].domain, "dbltest.com");
    assert_eq!(scanned.lookups[0].result, "listed:spam");
    assert!(ReputationCache::default_path().exists());
}

/// Asks Cloudflare's public resolver directly, which Spamhaus refuses.
struct PublicResolver;

impl DnsResolver for PublicResolver {
    fn lookup_a(&self, fqdn: &str) -> Result<DnsAnswer, String> {
        use hickory_resolver::TokioAsyncResolver;
        use hickory_resolver::config::{ResolverConfig, ResolverOpts};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut opts = ResolverOpts::default();
            opts.timeout = Duration::from_secs(3);
            let resolver = TokioAsyncResolver::tokio(ResolverConfig::cloudflare(), opts);
            resolver
                .ipv4_lookup(fqdn)
                .await
                .map(|l| DnsAnswer::A(l.iter().map(|a| a.0).collect()))
                .map_err(|_| "DNS lookup failed".to_string())
        })
    }
}

#[test]
#[ignore = "live DNS: queries Spamhaus through 1.1.1.1"]
fn public_resolver_refusal_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let log = LookupLog::default();
    let analyzers: Vec<Box<dyn Analyzer>> = vec![Box::new(ReputationAnalyzer::new(
        Box::new(PublicResolver),
        None,
        ReputationCache::new(dir.path().join(CACHE_FILE_NAME)),
        log.clone(),
    ))];
    let mut input = ThreatInput::from_raw(FIXTURE, "me@example.org").unwrap();
    input.ledger = Ok(Default::default());
    let verdict = evaluate(&input, &analyzers, &ThreatConfig::default());
    println!("{verdict:#?}");
    assert_eq!(verdict.level, Level::Unavailable);
    assert!(
        verdict.analyzers_skipped[0]
            .reason
            .contains("127.255.255.254"),
        "{verdict:?}"
    );
    let records = log.lock().unwrap().clone();
    assert_eq!(records[0].domain, "dbltest.com");
    assert!(records[0].result.starts_with("unavailable:"));
}

#[tokio::test]
#[ignore = "live HTTPS: IANA bootstrap and Verisign RDAP"]
async fn rdap_finds_the_registrar_abuse_contact_for_paypal_com() {
    use envelope_email_transport::threat::rdap::{PublicRdap, abuse_contact};
    let found = abuse_contact(&PublicRdap, "paypal.com").await;
    println!("{found:?}");
    let email = found.result.expect("RDAP answered");
    assert!(email.contains('@'), "{email}");
    assert!(found.lookups.iter().all(|l| l.domain == "paypal.com"));
}
