// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::future::Future;
use std::time::Duration;

use anyhow::{Result, bail};
use hickory_resolver::TokioAsyncResolver;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::error::{ResolveError, ResolveErrorKind};
use hickory_resolver::lookup::{Ipv4Lookup, Ipv6Lookup, MxLookup, TxtLookup};
use serde::Serialize;

use crate::DeliverabilityCmd;

const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsSnapshot {
    pub domain: String,
    pub mx_records: Vec<String>,
    pub spf_records: Vec<String>,
    pub dmarc_records: Vec<String>,
    pub a_records: Vec<String>,
    pub aaaa_records: Vec<String>,
    pub lookup_failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeliverabilityReport {
    pub domain: String,
    pub status: DeliverabilityStatus,
    pub checks: Vec<DeliverabilityCheck>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeliverabilityCheck {
    pub name: String,
    pub status: CheckStatus,
    pub summary: String,
    pub records: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeliverabilityStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warning,
    Error,
}

#[tokio::main]
pub async fn run(subcommand: DeliverabilityCmd, json_output: bool) -> Result<()> {
    match subcommand {
        DeliverabilityCmd::Check { domain } => {
            let domain = normalize_domain(&domain)?;
            let snapshot = resolve_dns_snapshot(&domain).await;
            let report = build_report(snapshot);
            print_report(&report, json_output)
        }
    }
}

pub fn build_report(snapshot: DnsSnapshot) -> DeliverabilityReport {
    let DnsSnapshot {
        domain,
        mx_records,
        spf_records,
        dmarc_records,
        a_records,
        aaaa_records,
        lookup_failures,
    } = snapshot;

    let lookup_failed = |name: &str| lookup_failures.iter().any(|failure| failure == name);
    let has_a = !a_records.is_empty();
    let has_aaaa = !aaaa_records.is_empty();
    let has_address = has_a || has_aaaa;

    let mut checks = Vec::with_capacity(5);
    let mut recommendations = Vec::new();

    if lookup_failed("mx") {
        checks.push(DeliverabilityCheck {
            name: "mx".to_string(),
            status: CheckStatus::Error,
            summary: "MX lookup failed; DNS resolver did not return a usable response.".to_string(),
            records: mx_records,
        });
        recommendations.push(format!(
            "Retry the MX lookup for {domain} from a working DNS resolver before changing records."
        ));
    } else if mx_records.is_empty() {
        checks.push(DeliverabilityCheck {
            name: "mx".to_string(),
            status: CheckStatus::Error,
            summary: "No MX records found; this domain cannot receive mail directly.".to_string(),
            records: mx_records,
        });
        recommendations.push(format!(
            "Add at least one MX record for {domain} pointing at the mail exchanger you operate."
        ));
    } else {
        checks.push(DeliverabilityCheck {
            name: "mx".to_string(),
            status: CheckStatus::Ok,
            summary: plural_summary(mx_records.len(), "MX record found", "MX records found"),
            records: mx_records,
        });
    }

    if lookup_failed("spf") {
        checks.push(DeliverabilityCheck {
            name: "spf".to_string(),
            status: CheckStatus::Error,
            summary: "SPF TXT lookup failed; DNS resolver did not return a usable response."
                .to_string(),
            records: spf_records,
        });
        recommendations.push(format!(
            "Retry the TXT lookup for {domain} from a working DNS resolver before changing SPF."
        ));
    } else if spf_records.is_empty() {
        checks.push(DeliverabilityCheck {
            name: "spf".to_string(),
            status: CheckStatus::Warning,
            summary: "No SPF TXT record found on the domain.".to_string(),
            records: spf_records,
        });
        recommendations.push(format!(
            "Publish an SPF TXT record on {domain} that starts with v=spf1 and names your outbound mail hosts."
        ));
    } else {
        checks.push(DeliverabilityCheck {
            name: "spf".to_string(),
            status: CheckStatus::Ok,
            summary: plural_summary(spf_records.len(), "SPF record found", "SPF records found"),
            records: spf_records,
        });
    }

    if lookup_failed("dmarc") {
        checks.push(DeliverabilityCheck {
            name: "dmarc".to_string(),
            status: CheckStatus::Error,
            summary: "DMARC TXT lookup failed; DNS resolver did not return a usable response."
                .to_string(),
            records: dmarc_records,
        });
        recommendations.push(format!(
            "Retry the TXT lookup for _dmarc.{domain} from a working DNS resolver before changing DMARC."
        ));
    } else if dmarc_records.is_empty() {
        checks.push(DeliverabilityCheck {
            name: "dmarc".to_string(),
            status: CheckStatus::Warning,
            summary: "No DMARC TXT record found.".to_string(),
            records: dmarc_records,
        });
        recommendations.push(format!(
            "Add a DMARC TXT record at _dmarc.{domain}; start with p=none while validating mail flow."
        ));
    } else {
        checks.push(DeliverabilityCheck {
            name: "dmarc".to_string(),
            status: CheckStatus::Ok,
            summary: plural_summary(
                dmarc_records.len(),
                "DMARC record found",
                "DMARC records found",
            ),
            records: dmarc_records,
        });
    }

    if lookup_failed("a") {
        checks.push(DeliverabilityCheck {
            name: "a".to_string(),
            status: CheckStatus::Error,
            summary: "A lookup failed; DNS resolver did not return a usable response.".to_string(),
            records: a_records,
        });
    } else if has_a {
        checks.push(DeliverabilityCheck {
            name: "a".to_string(),
            status: CheckStatus::Ok,
            summary: plural_summary(a_records.len(), "A record found", "A records found"),
            records: a_records,
        });
    } else {
        checks.push(DeliverabilityCheck {
            name: "a".to_string(),
            status: address_check_status(has_address),
            summary: address_missing_summary("A", has_address),
            records: a_records,
        });
    }

    if lookup_failed("aaaa") {
        checks.push(DeliverabilityCheck {
            name: "aaaa".to_string(),
            status: CheckStatus::Error,
            summary: "AAAA lookup failed; DNS resolver did not return a usable response."
                .to_string(),
            records: aaaa_records,
        });
    } else if has_aaaa {
        checks.push(DeliverabilityCheck {
            name: "aaaa".to_string(),
            status: CheckStatus::Ok,
            summary: plural_summary(
                aaaa_records.len(),
                "AAAA record found",
                "AAAA records found",
            ),
            records: aaaa_records,
        });
    } else {
        checks.push(DeliverabilityCheck {
            name: "aaaa".to_string(),
            status: address_check_status(has_address),
            summary: address_missing_summary("AAAA", has_address),
            records: aaaa_records,
        });
    }

    if !has_address && !lookup_failed("a") && !lookup_failed("aaaa") {
        recommendations.push(format!(
            "Add an A or AAAA record for {domain} so the bare domain has basic host DNS posture."
        ));
    }

    let status = report_status(&checks);

    DeliverabilityReport {
        domain,
        status,
        checks,
        recommendations,
    }
}

async fn resolve_dns_snapshot(domain: &str) -> DnsSnapshot {
    let resolver = build_resolver();
    let domain_name = fqdn(domain);
    let dmarc_name = fqdn(&format!("_dmarc.{domain}"));

    let (
        (mx_records, mx_lookup_failed),
        (txt_records, txt_lookup_failed),
        (dmarc_txt_records, dmarc_lookup_failed),
        (a_records, a_lookup_failed),
        (aaaa_records, aaaa_lookup_failed),
    ) = tokio::join!(
        lookup_mx_records(&resolver, &domain_name),
        lookup_txt_records(&resolver, &domain_name),
        lookup_txt_records(&resolver, &dmarc_name),
        lookup_a_records(&resolver, &domain_name),
        lookup_aaaa_records(&resolver, &domain_name),
    );

    let mut lookup_failures = Vec::new();
    if mx_lookup_failed {
        lookup_failures.push("mx".to_string());
    }
    if txt_lookup_failed {
        lookup_failures.push("spf".to_string());
    }
    if dmarc_lookup_failed {
        lookup_failures.push("dmarc".to_string());
    }
    if a_lookup_failed {
        lookup_failures.push("a".to_string());
    }
    if aaaa_lookup_failed {
        lookup_failures.push("aaaa".to_string());
    }

    DnsSnapshot {
        domain: domain.to_string(),
        mx_records,
        spf_records: filter_txt_records(txt_records, "v=spf1"),
        dmarc_records: filter_txt_records(dmarc_txt_records, "v=dmarc1"),
        a_records,
        aaaa_records,
        lookup_failures,
    }
}

fn build_resolver() -> TokioAsyncResolver {
    let (config, mut opts) = hickory_resolver::system_conf::read_system_conf()
        .unwrap_or_else(|_| (ResolverConfig::default(), ResolverOpts::default()));
    opts.timeout = DNS_LOOKUP_TIMEOUT;
    opts.attempts = opts.attempts.clamp(1, 2);

    TokioAsyncResolver::tokio(config, opts)
}

async fn lookup_with_timeout<T, F>(future: F) -> std::result::Result<Option<T>, ()>
where
    F: Future<Output = std::result::Result<T, ResolveError>>,
{
    match tokio::time::timeout(DNS_LOOKUP_TIMEOUT, future).await {
        Ok(Ok(records)) => Ok(Some(records)),
        Ok(Err(error)) if is_no_records(&error) => Ok(None),
        Ok(Err(_)) | Err(_) => Err(()),
    }
}

fn is_no_records(error: &ResolveError) -> bool {
    matches!(error.kind(), ResolveErrorKind::NoRecordsFound { .. })
}

async fn lookup_mx_records(resolver: &TokioAsyncResolver, name: &str) -> (Vec<String>, bool) {
    match lookup_with_timeout::<MxLookup, _>(resolver.mx_lookup(name)).await {
        Ok(Some(lookup)) => {
            let mut records: Vec<String> = lookup
                .iter()
                .map(|record| {
                    format!(
                        "{} {}",
                        record.preference(),
                        trim_trailing_dot(&record.exchange().to_string())
                    )
                })
                .collect();
            records.sort();
            (records, false)
        }
        Ok(None) => (Vec::new(), false),
        Err(()) => (Vec::new(), true),
    }
}

async fn lookup_txt_records(resolver: &TokioAsyncResolver, name: &str) -> (Vec<String>, bool) {
    match lookup_with_timeout::<TxtLookup, _>(resolver.txt_lookup(name)).await {
        Ok(Some(lookup)) => {
            let mut records: Vec<String> = lookup.iter().map(txt_record_to_string).collect();
            records.sort();
            (records, false)
        }
        Ok(None) => (Vec::new(), false),
        Err(()) => (Vec::new(), true),
    }
}

async fn lookup_a_records(resolver: &TokioAsyncResolver, name: &str) -> (Vec<String>, bool) {
    match lookup_with_timeout::<Ipv4Lookup, _>(resolver.ipv4_lookup(name)).await {
        Ok(Some(lookup)) => {
            let mut records: Vec<String> =
                lookup.iter().map(|record| record.0.to_string()).collect();
            records.sort();
            (records, false)
        }
        Ok(None) => (Vec::new(), false),
        Err(()) => (Vec::new(), true),
    }
}

async fn lookup_aaaa_records(resolver: &TokioAsyncResolver, name: &str) -> (Vec<String>, bool) {
    match lookup_with_timeout::<Ipv6Lookup, _>(resolver.ipv6_lookup(name)).await {
        Ok(Some(lookup)) => {
            let mut records: Vec<String> =
                lookup.iter().map(|record| record.0.to_string()).collect();
            records.sort();
            (records, false)
        }
        Ok(None) => (Vec::new(), false),
        Err(()) => (Vec::new(), true),
    }
}

fn txt_record_to_string(record: &hickory_resolver::proto::rr::rdata::TXT) -> String {
    record
        .iter()
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join("")
}

fn filter_txt_records(records: Vec<String>, prefix: &str) -> Vec<String> {
    let prefix = prefix.to_ascii_lowercase();
    records
        .into_iter()
        .filter(|record| {
            record
                .trim_start()
                .to_ascii_lowercase()
                .starts_with(&prefix)
        })
        .collect()
}

fn normalize_domain(domain: &str) -> Result<String> {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        bail!("--domain must not be empty");
    }
    if domain.chars().any(char::is_whitespace) {
        bail!("--domain must not contain whitespace");
    }
    Ok(domain)
}

fn fqdn(domain: &str) -> String {
    format!("{domain}.")
}

fn trim_trailing_dot(value: &str) -> String {
    value.trim_end_matches('.').to_string()
}

fn plural_summary(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}.")
    } else {
        format!("{count} {plural}.")
    }
}

fn address_check_status(has_address: bool) -> CheckStatus {
    if has_address {
        CheckStatus::Ok
    } else {
        CheckStatus::Warning
    }
}

fn address_missing_summary(record_type: &str, has_address: bool) -> String {
    if has_address {
        format!("No {record_type} records found; another address record type is present.")
    } else {
        format!("No {record_type} records found.")
    }
}

fn report_status(checks: &[DeliverabilityCheck]) -> DeliverabilityStatus {
    if checks
        .iter()
        .any(|check| check.status == CheckStatus::Error)
    {
        return DeliverabilityStatus::Error;
    }

    if checks
        .iter()
        .any(|check| check.status == CheckStatus::Warning)
    {
        return DeliverabilityStatus::Warning;
    }

    DeliverabilityStatus::Ok
}

fn print_report(report: &DeliverabilityReport, json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }

    println!("Deliverability for {}: {:?}", report.domain, report.status);
    for check in &report.checks {
        println!("- {}: {:?} - {}", check.name, check.status, check.summary);
        for record in &check.records {
            println!("  {record}");
        }
    }

    if !report.recommendations.is_empty() {
        println!("Recommendations:");
        for recommendation in &report.recommendations {
            println!("- {recommendation}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check<'a>(report: &'a DeliverabilityReport, name: &str) -> &'a DeliverabilityCheck {
        report
            .checks
            .iter()
            .find(|check| check.name == name)
            .expect("expected check in report")
    }

    #[test]
    fn report_ok_when_mx_spf_dmarc_and_address_records_present() {
        let report = build_report(DnsSnapshot {
            domain: "example.com".to_string(),
            mx_records: vec!["10 mail.example.com".to_string()],
            spf_records: vec!["v=spf1 mx -all".to_string()],
            dmarc_records: vec!["v=DMARC1; p=quarantine".to_string()],
            a_records: vec!["192.0.2.10".to_string()],
            aaaa_records: vec!["2001:db8::10".to_string()],
            lookup_failures: Vec::new(),
        });

        assert_eq!(report.domain, "example.com");
        assert_eq!(report.status, DeliverabilityStatus::Ok);
        assert!(report.recommendations.is_empty());
        assert_eq!(check(&report, "mx").status, CheckStatus::Ok);
        assert_eq!(check(&report, "spf").status, CheckStatus::Ok);
        assert_eq!(check(&report, "dmarc").status, CheckStatus::Ok);
        assert_eq!(check(&report, "a").status, CheckStatus::Ok);
        assert_eq!(check(&report, "aaaa").status, CheckStatus::Ok);
    }

    #[test]
    fn report_warning_when_dmarc_missing() {
        let report = build_report(DnsSnapshot {
            domain: "example.com".to_string(),
            mx_records: vec!["10 mail.example.com".to_string()],
            spf_records: vec!["v=spf1 mx -all".to_string()],
            dmarc_records: Vec::new(),
            a_records: vec!["192.0.2.10".to_string()],
            aaaa_records: Vec::new(),
            lookup_failures: Vec::new(),
        });

        assert_eq!(report.status, DeliverabilityStatus::Warning);
        assert_eq!(check(&report, "dmarc").status, CheckStatus::Warning);
        assert!(
            report
                .recommendations
                .iter()
                .any(|recommendation| recommendation.contains("_dmarc.example.com"))
        );
    }

    #[test]
    fn report_error_when_mx_missing() {
        let report = build_report(DnsSnapshot {
            domain: "example.com".to_string(),
            mx_records: Vec::new(),
            spf_records: vec!["v=spf1 mx -all".to_string()],
            dmarc_records: vec!["v=DMARC1; p=quarantine".to_string()],
            a_records: vec!["192.0.2.10".to_string()],
            aaaa_records: Vec::new(),
            lookup_failures: Vec::new(),
        });

        assert_eq!(report.status, DeliverabilityStatus::Error);
        assert_eq!(check(&report, "mx").status, CheckStatus::Error);
        assert!(
            report
                .recommendations
                .iter()
                .any(|recommendation| recommendation.contains("MX"))
        );
    }

    #[test]
    fn report_error_when_mx_lookup_failed() {
        let report = build_report(DnsSnapshot {
            domain: "example.com".to_string(),
            mx_records: Vec::new(),
            spf_records: vec!["v=spf1 mx -all".to_string()],
            dmarc_records: vec!["v=DMARC1; p=quarantine".to_string()],
            a_records: vec!["192.0.2.10".to_string()],
            aaaa_records: Vec::new(),
            lookup_failures: vec!["mx".to_string()],
        });

        assert_eq!(report.status, DeliverabilityStatus::Error);
        assert_eq!(check(&report, "mx").status, CheckStatus::Error);
        assert!(check(&report, "mx").summary.contains("lookup failed"));
    }
}
