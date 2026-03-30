// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::collections::HashMap;
use std::time::Duration;

use envelope_email_store::models::DiscoveryResult;
use hickory_resolver::TokioAsyncResolver;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::errors::DiscoveryError;

/// Known MX base domain → canonical provider domain.
fn mx_aliases() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    m.insert("google.com", "gmail.com");
    m.insert("outlook.com", "office365.com");
    m.insert("protection.outlook.com", "office365.com");
    m.insert("microsoft.com", "office365.com");
    m
}

/// A candidate server endpoint discovered by one of the strategies.
#[derive(Debug, Clone)]
pub struct DiscoveryCandidate {
    pub host: String,
    pub port: u16,
    /// "smtp" or "imap"
    pub role: String,
    /// Lower is better: 0 = SRV, 1 = autoconfig, 2 = MX-derived, 3 = common patterns
    pub priority: u8,
    pub source: String,
}

/// Run auto-discovery for a domain and return the best SMTP + IMAP endpoints.
///
/// Tries strategies in priority order: SRV → Autoconfig → MX-derived → common patterns.
/// Each candidate is probed with a 3-second TCP connect before being accepted.
pub async fn discover(domain: &str) -> Result<DiscoveryResult, DiscoveryError> {
    let mut candidates = Vec::new();

    // Strategy 0: SRV records
    match discover_srv(domain).await {
        Ok(mut c) => candidates.append(&mut c),
        Err(e) => debug!("SRV discovery failed for {domain}: {e}"),
    }

    // Strategy 1: Autoconfig XML (deferred — requires HTTP client dependency)
    // Mozilla autoconfig: https://autoconfig.{domain}/mail/config-v1.1.xml
    // SRV + MX + common patterns cover the major providers without an HTTP dep.
    debug!("autoconfig skipped for {domain} (SRV/MX/common patterns used instead)");

    // Strategy 2: MX-derived
    match discover_mx(domain).await {
        Ok(mut c) => candidates.append(&mut c),
        Err(e) => debug!("MX discovery failed for {domain}: {e}"),
    }

    // Strategy 3: Common patterns
    candidates.append(&mut common_pattern_candidates(domain));

    if candidates.is_empty() {
        return Err(DiscoveryError::NoCandidates(domain.to_string()));
    }

    // Sort by priority (lower = better)
    candidates.sort_by_key(|c| c.priority);

    // Probe candidates and pick the best reachable SMTP + IMAP
    let smtp = probe_first(&candidates, "smtp").await;
    let imap = probe_first(&candidates, "imap").await;

    match (smtp, imap) {
        (Some(s), Some(i)) => {
            info!(
                "discovered {domain}: smtp={}:{} ({}), imap={}:{} ({})",
                s.host, s.port, s.source, i.host, i.port, i.source
            );
            Ok(DiscoveryResult {
                domain: domain.to_string(),
                smtp_host: s.host,
                smtp_port: s.port,
                smtp_source: s.source,
                imap_host: i.host,
                imap_port: i.port,
                imap_source: i.source,
            })
        }
        (None, Some(_)) => Err(DiscoveryError::NoCandidates(format!(
            "no reachable SMTP server found for {domain}"
        ))),
        (Some(_), None) => Err(DiscoveryError::NoCandidates(format!(
            "no reachable IMAP server found for {domain}"
        ))),
        (None, None) => Err(DiscoveryError::NoCandidates(format!(
            "no reachable servers found for {domain}"
        ))),
    }
}

/// Probe candidates of a given role and return the first one that accepts a TCP connection.
async fn probe_first(candidates: &[DiscoveryCandidate], role: &str) -> Option<DiscoveryCandidate> {
    for c in candidates.iter().filter(|c| c.role == role) {
        if probe_tcp(&c.host, c.port).await {
            return Some(c.clone());
        }
    }
    None
}

/// Try connecting to host:port with a 3-second timeout.
async fn probe_tcp(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");
    debug!("probing {addr}");
    match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => {
            debug!("probe {addr} succeeded");
            true
        }
        Ok(Err(e)) => {
            debug!("probe {addr} failed: {e}");
            false
        }
        Err(_) => {
            debug!("probe {addr} timed out");
            false
        }
    }
}

/// Discover SMTP/IMAP via SRV records.
async fn discover_srv(domain: &str) -> Result<Vec<DiscoveryCandidate>, DiscoveryError> {
    let resolver = TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default());

    let mut candidates = Vec::new();

    // _submissions._tcp.{domain} → SMTP submission (port 465)
    let smtp_srv = format!("_submissions._tcp.{domain}.");
    match resolver.srv_lookup(&smtp_srv).await {
        Ok(lookup) => {
            for record in lookup.iter() {
                let host = record
                    .target()
                    .to_string()
                    .trim_end_matches('.')
                    .to_string();
                candidates.push(DiscoveryCandidate {
                    host,
                    port: record.port(),
                    role: "smtp".into(),
                    priority: 0,
                    source: "srv".into(),
                });
            }
        }
        Err(e) => debug!("SRV lookup {smtp_srv} failed: {e}"),
    }

    // _imaps._tcp.{domain} → IMAP over TLS (port 993)
    let imap_srv = format!("_imaps._tcp.{domain}.");
    match resolver.srv_lookup(&imap_srv).await {
        Ok(lookup) => {
            for record in lookup.iter() {
                let host = record
                    .target()
                    .to_string()
                    .trim_end_matches('.')
                    .to_string();
                candidates.push(DiscoveryCandidate {
                    host,
                    port: record.port(),
                    role: "imap".into(),
                    priority: 0,
                    source: "srv".into(),
                });
            }
        }
        Err(e) => debug!("SRV lookup {imap_srv} failed: {e}"),
    }

    Ok(candidates)
}

/// Discover SMTP/IMAP by resolving MX records and deriving hostnames.
async fn discover_mx(domain: &str) -> Result<Vec<DiscoveryCandidate>, DiscoveryError> {
    let resolver = TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default());

    let mx_lookup = resolver
        .mx_lookup(domain)
        .await
        .map_err(|e| DiscoveryError::Dns(format!("MX lookup for {domain}: {e}")))?;

    let aliases = mx_aliases();
    let mut candidates = Vec::new();

    for record in mx_lookup.iter() {
        let mx_host = record
            .exchange()
            .to_string()
            .trim_end_matches('.')
            .to_string();

        // Extract base domain from MX hostname (last two labels)
        let mx_base = extract_base_domain(&mx_host);
        debug!("MX {mx_host} → base domain {mx_base}");

        // Apply alias mapping
        let provider = aliases.get(mx_base.as_str()).copied().unwrap_or(&mx_base);

        candidates.push(DiscoveryCandidate {
            host: format!("smtp.{provider}"),
            port: 465,
            role: "smtp".into(),
            priority: 2,
            source: format!("mx:{mx_host}"),
        });

        candidates.push(DiscoveryCandidate {
            host: format!("smtp.{provider}"),
            port: 587,
            role: "smtp".into(),
            priority: 2,
            source: format!("mx:{mx_host}"),
        });

        candidates.push(DiscoveryCandidate {
            host: format!("imap.{provider}"),
            port: 993,
            role: "imap".into(),
            priority: 2,
            source: format!("mx:{mx_host}"),
        });
    }

    Ok(candidates)
}

/// Extract the base domain (last two labels) from a hostname.
fn extract_base_domain(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() >= 2 {
        format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        host.to_string()
    }
}

/// Generate common-pattern candidates for a domain.
fn common_pattern_candidates(domain: &str) -> Vec<DiscoveryCandidate> {
    vec![
        DiscoveryCandidate {
            host: format!("smtp.{domain}"),
            port: 465,
            role: "smtp".into(),
            priority: 3,
            source: "common".into(),
        },
        DiscoveryCandidate {
            host: format!("smtp.{domain}"),
            port: 587,
            role: "smtp".into(),
            priority: 3,
            source: "common".into(),
        },
        DiscoveryCandidate {
            host: format!("mail.{domain}"),
            port: 465,
            role: "smtp".into(),
            priority: 3,
            source: "common".into(),
        },
        DiscoveryCandidate {
            host: format!("mail.{domain}"),
            port: 587,
            role: "smtp".into(),
            priority: 3,
            source: "common".into(),
        },
        DiscoveryCandidate {
            host: format!("imap.{domain}"),
            port: 993,
            role: "imap".into(),
            priority: 3,
            source: "common".into(),
        },
        DiscoveryCandidate {
            host: format!("mail.{domain}"),
            port: 993,
            role: "imap".into(),
            priority: 3,
            source: "common".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_base_domain() {
        assert_eq!(extract_base_domain("mx1.mail.google.com"), "google.com");
        assert_eq!(extract_base_domain("example.com"), "example.com");
        assert_eq!(
            extract_base_domain("smtp2.messagingengine.com"),
            "messagingengine.com"
        );
    }

    #[test]
    fn test_common_patterns_generates_both_roles() {
        let candidates = common_pattern_candidates("example.com");
        assert!(candidates.iter().any(|c| c.role == "smtp"));
        assert!(candidates.iter().any(|c| c.role == "imap"));
        assert!(candidates.iter().all(|c| c.priority == 3));
    }
}
