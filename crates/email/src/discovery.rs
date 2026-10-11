// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use envelope_email_store::models::DiscoveryResult;
use hickory_resolver::TokioAsyncResolver;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::error::{ResolveErrorKind, ResolveResult};
use hickory_resolver::lookup::Lookup;
use hickory_resolver::proto::op::ResponseCode;
use hickory_resolver::proto::rr::RecordType;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

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
    let sources = dns_sources(
        hickory_resolver::system_conf::read_system_conf(),
        (ResolverConfig::default(), ResolverOpts::default()),
    );
    discover_with(domain, &sources, probe_tcp).await
}

/// [`discover`] with injectable DNS sources and probe, so tests never touch
/// the network.
async fn discover_with<P, F>(
    domain: &str,
    sources: &[DnsSource],
    probe: P,
) -> Result<DiscoveryResult, DiscoveryError>
where
    P: Fn(String, u16) -> F,
    F: Future<Output = bool>,
{
    let mut candidates = Vec::new();
    let mut dns_failures = Vec::new();

    // Strategy 0: SRV records
    candidates.append(&mut discover_srv(domain, sources, &mut dns_failures).await);

    // Strategy 1: Autoconfig XML (deferred — requires HTTP client dependency)
    // Mozilla autoconfig: https://autoconfig.{domain}/mail/config-v1.1.xml
    // SRV + MX + common patterns cover the major providers without an HTTP dep.
    debug!("autoconfig skipped for {domain} (SRV/MX/common patterns used instead)");

    // Strategy 2: MX-derived
    candidates.append(&mut discover_mx(domain, sources, &mut dns_failures).await);

    // Strategy 3: Common patterns
    candidates.append(&mut common_pattern_candidates(domain));

    if candidates.is_empty() {
        return Err(DiscoveryError::NoCandidates(domain.to_string()));
    }

    // Sort by priority (lower = better)
    candidates.sort_by_key(|c| c.priority);

    // Probe candidates and pick the best reachable SMTP + IMAP
    let smtp = probe_first(&candidates, "smtp", &probe).await;
    let imap = probe_first(&candidates, "imap", &probe).await;

    let missing = match (smtp, imap) {
        (Some(s), Some(i)) => {
            info!(
                "discovered {domain}: smtp={}:{} ({}), imap={}:{} ({})",
                s.host, s.port, s.source, i.host, i.port, i.source
            );
            return Ok(DiscoveryResult {
                domain: domain.to_string(),
                smtp_host: s.host,
                smtp_port: s.port,
                smtp_source: s.source,
                imap_host: i.host,
                imap_port: i.port,
                imap_source: i.source,
            });
        }
        (None, Some(_)) => format!("no reachable SMTP server found for {domain}"),
        (Some(_), None) => format!("no reachable IMAP server found for {domain}"),
        (None, None) => format!("no reachable servers found for {domain}"),
    };
    if dns_failures.is_empty() {
        return Err(DiscoveryError::NoCandidates(missing));
    }
    Err(DiscoveryError::Dns(format!(
        "{missing}; {}",
        describe_dns_failures(&dns_failures)
    )))
}

/// A set of DNS servers discovery asks, labelled for error messages.
struct DnsSource {
    label: String,
    resolver: TokioAsyncResolver,
}

impl DnsSource {
    fn new(kind: &str, config: ResolverConfig, opts: ResolverOpts) -> Self {
        let mut ips: Vec<String> = Vec::new();
        for ns in config.name_servers() {
            let ip = ns.socket_addr.ip().to_string();
            if !ips.contains(&ip) {
                ips.push(ip);
            }
        }
        Self {
            label: format!("{kind} ({})", ips.join(", ")),
            resolver: TokioAsyncResolver::tokio(config, opts),
        }
    }
}

/// The resolvers discovery asks, in order: the system's own (from
/// /etc/resolv.conf, which macOS keeps in step with its DNS settings), then
/// the public one. IMAP and SMTP connections resolve hosts through the system
/// resolver anyway, and a VPN that blocks direct DNS leaves only that one
/// answering.
fn dns_sources(
    system: ResolveResult<(ResolverConfig, ResolverOpts)>,
    public: (ResolverConfig, ResolverOpts),
) -> Vec<DnsSource> {
    let mut sources = Vec::new();
    match system {
        Ok((config, opts)) => sources.push(DnsSource::new("system resolver", config, opts)),
        Err(e) => warn!(
            "could not read the system DNS configuration ({e}); discovery asks the public resolver only"
        ),
    }
    sources.push(DnsSource::new("public resolver", public.0, public.1));
    sources
}

/// A DNS question that no resolver answered, with each resolver's error.
struct DnsFailure {
    question: String,
    errors: Vec<(String, String)>,
}

/// Ask each source in turn until one answers. `Ok(None)` means a resolver
/// said the name has no such records (NXDOMAIN, or NOERROR with no data);
/// that answer is final. A resolver that times out, errors, or answers
/// SERVFAIL or REFUSED (which hickory also reports as `NoRecordsFound`)
/// passes the question on to the next.
async fn ask(
    sources: &[DnsSource],
    name: &str,
    record_type: RecordType,
) -> Result<Option<Lookup>, DnsFailure> {
    let mut errors = Vec::new();
    for source in sources {
        match source.resolver.lookup(name, record_type).await {
            Ok(lookup) => {
                debug!("{record_type} {name} answered by {}", source.label);
                return Ok(Some(lookup));
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ResolveErrorKind::NoRecordsFound {
                        response_code: ResponseCode::NXDomain | ResponseCode::NoError,
                        ..
                    }
                ) =>
            {
                debug!("no {record_type} records for {name} ({})", source.label);
                return Ok(None);
            }
            Err(e) => {
                debug!(
                    "{record_type} lookup {name} via {} failed: {e}",
                    source.label
                );
                errors.push((source.label.clone(), e.to_string()));
            }
        }
    }
    Err(DnsFailure {
        question: format!("{record_type} {}", name.trim_end_matches('.')),
        errors,
    })
}

/// Name the unanswered questions and each resolver tried, with the first
/// error it gave.
fn describe_dns_failures(failures: &[DnsFailure]) -> String {
    let questions: Vec<&str> = failures.iter().map(|f| f.question.as_str()).collect();
    let mut tried: Vec<String> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (label, error) in failures.iter().flat_map(|f| &f.errors) {
        if !seen.contains(&label.as_str()) {
            seen.push(label);
            tried.push(format!("{label}: {error}"));
        }
    }
    format!(
        "no DNS resolver answered {}. Resolvers tried: {}",
        questions.join(", "),
        tried.join("; ")
    )
}

/// Probe candidates of a given role and return the first one that accepts a TCP connection.
async fn probe_first<P, F>(
    candidates: &[DiscoveryCandidate],
    role: &str,
    probe: &P,
) -> Option<DiscoveryCandidate>
where
    P: Fn(String, u16) -> F,
    F: Future<Output = bool>,
{
    for c in candidates.iter().filter(|c| c.role == role) {
        if probe(c.host.clone(), c.port).await {
            return Some(c.clone());
        }
    }
    None
}

/// Try connecting to host:port with a 3-second timeout.
async fn probe_tcp(host: String, port: u16) -> bool {
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
async fn discover_srv(
    domain: &str,
    sources: &[DnsSource],
    failures: &mut Vec<DnsFailure>,
) -> Vec<DiscoveryCandidate> {
    let mut candidates = Vec::new();

    // _submissions._tcp.{domain} → SMTP submission (port 465)
    let smtp_srv = format!("_submissions._tcp.{domain}.");
    match ask(sources, &smtp_srv, RecordType::SRV).await {
        Ok(Some(lookup)) => {
            for record in lookup.iter().filter_map(|r| r.as_srv()) {
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
        Ok(None) => {}
        Err(failure) => failures.push(failure),
    }

    // _imaps._tcp.{domain} → IMAP over TLS (port 993)
    let imap_srv = format!("_imaps._tcp.{domain}.");
    match ask(sources, &imap_srv, RecordType::SRV).await {
        Ok(Some(lookup)) => {
            for record in lookup.iter().filter_map(|r| r.as_srv()) {
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
        Ok(None) => {}
        Err(failure) => failures.push(failure),
    }

    candidates
}

/// Discover SMTP/IMAP by resolving MX records and deriving hostnames.
async fn discover_mx(
    domain: &str,
    sources: &[DnsSource],
    failures: &mut Vec<DnsFailure>,
) -> Vec<DiscoveryCandidate> {
    // Fully qualified, so a system search domain is never appended.
    let mx_lookup = match ask(sources, &format!("{domain}."), RecordType::MX).await {
        Ok(Some(lookup)) => lookup,
        Ok(None) => return Vec::new(),
        Err(failure) => {
            failures.push(failure);
            return Vec::new();
        }
    };

    let aliases = mx_aliases();
    let mut candidates = Vec::new();

    for record in mx_lookup.iter().filter_map(|r| r.as_mx()) {
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

    candidates
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
    use std::net::SocketAddr;

    use hickory_resolver::config::{NameServerConfig, Protocol};
    use hickory_resolver::proto::op::{Message, MessageType, Query};
    use hickory_resolver::proto::rr::rdata::SRV;
    use hickory_resolver::proto::rr::{Name, RData, Record};
    use tokio::net::UdpSocket;

    /// A DNS server on loopback that answers each query with `answer`.
    async fn spawn_dns(answer: fn(&Query) -> (ResponseCode, Vec<Record>)) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok((len, peer)) = socket.recv_from(&mut buf).await {
                let request = Message::from_vec(&buf[..len]).unwrap();
                let mut reply = Message::new();
                reply
                    .set_id(request.id())
                    .set_message_type(MessageType::Response)
                    .set_recursion_desired(request.recursion_desired())
                    .set_recursion_available(true);
                let (code, records) = answer(&request.queries()[0]);
                reply.set_response_code(code);
                reply.add_query(request.queries()[0].clone());
                reply.add_answers(records);
                socket
                    .send_to(&reply.to_vec().unwrap(), peer)
                    .await
                    .unwrap();
            }
        });
        addr
    }

    /// SRV records for example.test, the way a mail host publishes them.
    fn example_srv(query: &Query) -> (ResponseCode, Vec<Record>) {
        let (target, port) = match (query.query_type(), query.name().to_ascii().as_str()) {
            (RecordType::SRV, "_submissions._tcp.example.test.") => {
                ("smtp.mail.example.test.", 465)
            }
            (RecordType::SRV, "_imaps._tcp.example.test.") => ("imap.mail.example.test.", 993),
            _ => return (ResponseCode::NXDomain, Vec::new()),
        };
        let srv = SRV::new(0, 0, port, Name::from_ascii(target).unwrap());
        let record = Record::from_rdata(query.name().clone(), 60, RData::SRV(srv));
        (ResponseCode::NoError, vec![record])
    }

    fn nxdomain(_: &Query) -> (ResponseCode, Vec<Record>) {
        (ResponseCode::NXDomain, Vec::new())
    }

    fn servfail(_: &Query) -> (ResponseCode, Vec<Record>) {
        (ResponseCode::ServFail, Vec::new())
    }

    /// A resolver configuration for one UDP server, failing fast.
    fn udp_resolver(addr: SocketAddr) -> (ResolverConfig, ResolverOpts) {
        let mut config = ResolverConfig::new();
        config.add_name_server(NameServerConfig::new(addr, Protocol::Udp));
        let mut opts = ResolverOpts::default();
        opts.timeout = Duration::from_millis(300);
        opts.attempts = 1;
        (config, opts)
    }

    /// A probe that reaches only the hosts example.test's SRV records name.
    fn mail_hosts_reachable(host: String, port: u16) -> std::future::Ready<bool> {
        std::future::ready(matches!(
            (host.as_str(), port),
            ("smtp.mail.example.test", 465) | ("imap.mail.example.test", 993)
        ))
    }

    #[tokio::test]
    async fn discovery_uses_the_system_resolver_when_public_dns_is_unreachable() {
        let system_dns = spawn_dns(example_srv).await;
        // A VPN that blocks direct DNS: queries to the public resolver vanish.
        let blackhole = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sources = dns_sources(
            Ok(udp_resolver(system_dns)),
            udp_resolver(blackhole.local_addr().unwrap()),
        );

        let result = discover_with("example.test", &sources, mail_hosts_reachable)
            .await
            .expect("discovery should succeed through the system resolver");

        assert_eq!(
            (
                result.smtp_host.as_str(),
                result.smtp_port,
                result.smtp_source.as_str()
            ),
            ("smtp.mail.example.test", 465, "srv")
        );
        assert_eq!(
            (
                result.imap_host.as_str(),
                result.imap_port,
                result.imap_source.as_str()
            ),
            ("imap.mail.example.test", 993, "srv")
        );
    }

    #[tokio::test]
    async fn a_system_resolver_saying_no_records_is_final() {
        let system_dns = spawn_dns(nxdomain).await;
        let blackhole = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sources = dns_sources(
            Ok(udp_resolver(system_dns)),
            udp_resolver(blackhole.local_addr().unwrap()),
        );

        let err = discover_with("example.test", &sources, mail_hosts_reachable)
            .await
            .unwrap_err();

        // A DNS failure here would mean the question went on to the public
        // resolver after the system one had already answered.
        assert!(
            matches!(err, DiscoveryError::NoCandidates(_)),
            "expected no candidates, got {err}"
        );
    }

    #[tokio::test]
    async fn a_failing_system_resolver_hands_the_question_to_the_public_one() {
        let system_dns = spawn_dns(servfail).await;
        let public_dns = spawn_dns(example_srv).await;
        let sources = dns_sources(Ok(udp_resolver(system_dns)), udp_resolver(public_dns));

        let result = discover_with("example.test", &sources, mail_hosts_reachable)
            .await
            .expect("discovery should succeed through the public resolver");

        assert_eq!(result.imap_host, "imap.mail.example.test");
        assert_eq!(result.smtp_host, "smtp.mail.example.test");
    }

    #[tokio::test]
    async fn discovery_names_every_resolver_when_dns_fails_everywhere() {
        let system_blackhole = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let public_blackhole = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sources = dns_sources(
            Ok(udp_resolver(system_blackhole.local_addr().unwrap())),
            udp_resolver(public_blackhole.local_addr().unwrap()),
        );

        let err = discover_with("example.test", &sources, mail_hosts_reachable)
            .await
            .unwrap_err();

        let DiscoveryError::Dns(message) = &err else {
            panic!("expected a DNS failure, got {err}");
        };
        for expected in [
            "no reachable servers found for example.test",
            "SRV _submissions._tcp.example.test,",
            "SRV _imaps._tcp.example.test,",
            "MX example.test.",
            "system resolver (127.0.0.1): request timed out",
            "public resolver (127.0.0.1): request timed out",
        ] {
            assert!(
                message.contains(expected),
                "{expected:?} missing from: {message}"
            );
        }
    }

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
