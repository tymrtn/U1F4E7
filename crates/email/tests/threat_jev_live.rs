// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Live Jev check against the configured decisions provider. Ignored by
//! default because it makes one paid hosted call (about $0.00002).
//!
//! ```bash
//! export ENVELOPE_HOME=$(mktemp -d)
//! envelope config set threat.analyzers.jev true
//! cargo test -p envelope-email-transport --test threat_jev_live -- --ignored --nocapture
//! ```
//!
//! Needs the provider's key variable (`OPENROUTER_API_KEY` by default).

use envelope_email_store::Database;
use envelope_email_transport::threat::persist::{self, VerdictTarget};
use envelope_email_transport::threat::{ThreatConfig, explain};

const FIXTURE: &[u8] = include_bytes!("fixtures/jev-phish.eml");

#[test]
#[ignore = "live: one hosted decisions call"]
fn configured_jev_flags_a_phishing_fixture_and_logs_one_lookup() {
    let home = std::env::var("ENVELOPE_HOME").expect("set ENVELOPE_HOME to a scratch dir");
    let config = ThreatConfig::load().expect("config.json readable");
    assert!(
        config.jev,
        "run `envelope config set threat.analyzers.jev true` with ENVELOPE_HOME={home}"
    );

    let db = Database::open_default().expect("scratch database opens");
    let (verdict, scanned) = persist::scan_raw(&db, "acct", "me@example.org", FIXTURE, &config);
    for line in explain(&verdict) {
        println!("{line}");
    }
    let target = VerdictTarget {
        account_id: "acct",
        folder: "INBOX",
        uid: 1,
        message_id: scanned.message_id.as_deref(),
    };
    persist::record_verdict(&db, &target, &verdict).unwrap();
    persist::record_lookups(&db, &target, &scanned.lookups).unwrap();

    assert!(
        verdict.analyzers_run.contains(&"jev".to_string()),
        "{:?}",
        verdict.analyzers_skipped
    );
    assert!(
        verdict
            .signals
            .iter()
            .any(|s| s.code == "jev_phishing_risk"),
        "{verdict:?}"
    );

    let mut stmt = db
        .conn()
        .prepare("SELECT payload FROM events WHERE event_type = 'lookup_performed'")
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    println!("lookup_performed rows: {rows:?}");
    assert_eq!(rows.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
    assert_eq!(payload["provider"], "openrouter");
    assert_eq!(payload["result"], "answered");
    for leaked in ["paypa1", "password", "Dear Customer", "limited"] {
        assert!(!rows[0].contains(leaked), "{leaked} leaked");
    }
}
