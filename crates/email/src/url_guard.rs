// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Shared SSRF guard for user-supplied webhook URLs.
//!
//! Every sink that accepts an outbound webhook URL (CLI `events routes add`,
//! CLI `rule create/edit`, and the dashboard rule-write endpoints) must run
//! [`check_public_url`] before persisting so a private/reserved target can
//! never be reached from the server.

use url::{Host, Url};

/// Reasons a webhook URL is rejected. Callers map these to their own error
/// surface (anyhow context for the CLI, HTTP 400 + stable code for the
/// dashboard). The `Display` text is user-facing and preserves the exact
/// wording the CLI `events routes add` path shipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlGuardError {
    /// The string did not parse as a URL.
    Malformed(String),
    /// The scheme is not `http`/`https`.
    UnsupportedScheme(String),
    /// The URL had no host component.
    MissingHost,
    /// The host is `localhost` (any case).
    Localhost,
    /// The host is a literal IP in a private/reserved range.
    PrivateAddress(String),
}

impl std::fmt::Display for UrlGuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UrlGuardError::Malformed(url) => write!(f, "invalid webhook URL: {url}"),
            UrlGuardError::UnsupportedScheme(scheme) => {
                write!(f, "webhook URL must use http or https (got {scheme:?})")
            }
            UrlGuardError::MissingHost => write!(f, "webhook URL has no host"),
            UrlGuardError::Localhost => write!(
                f,
                "webhook URL host must be a public address \
                 (localhost is not permitted)"
            ),
            UrlGuardError::PrivateAddress(host) => write!(
                f,
                "webhook URL host {host} is a private/reserved address and is not permitted"
            ),
        }
    }
}

impl std::error::Error for UrlGuardError {}

/// Reject webhook URLs that would cause server-side request forgery.
///
/// Only `http` and `https` are permitted. Literal IP addresses in loopback,
/// link-local, private, or documentation blocks are rejected at parse time.
/// Named hosts that are not literal IPs are accepted here; a nameserver that
/// returns a private address (DNS rebinding) would bypass this check —
/// operators who need defence-in-depth for that window should run Envelope
/// behind a network egress policy.
pub fn check_public_url(raw_url: &str) -> Result<(), UrlGuardError> {
    let parsed = Url::parse(raw_url).map_err(|_| UrlGuardError::Malformed(raw_url.to_string()))?;

    // Only HTTP(S) schemes are meaningful for webhooks.
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => return Err(UrlGuardError::UnsupportedScheme(scheme.to_string())),
    }

    match parsed.host() {
        None => return Err(UrlGuardError::MissingHost),
        Some(Host::Domain(name)) => {
            // Reject bare `localhost` (case-insensitive). The url crate
            // normalises ASCII to lowercase for HTTP(S), so this covers it.
            if name.eq_ignore_ascii_case("localhost") {
                return Err(UrlGuardError::Localhost);
            }
        }
        Some(Host::Ipv4(v4)) => check_ipv4_ssrf(v4)?,
        Some(Host::Ipv6(v6)) => check_ipv6_ssrf(v6)?,
    }

    Ok(())
}

/// Return `Err` for IPv4 addresses in ranges that must not receive outbound
/// webhooks: loopback (127/8), link-local (169.254/16, incl. AWS/GCP
/// metadata), private (RFC 1918), documentation (RFC 5737 TEST-NETs),
/// broadcast (255.255.255.255), and unspecified (0.0.0.0).
fn check_ipv4_ssrf(v4: std::net::Ipv4Addr) -> Result<(), UrlGuardError> {
    let blocked = v4.is_loopback()
        || v4.is_link_local()
        || v4.is_private()
        || v4.is_unspecified()
        || v4 == std::net::Ipv4Addr::BROADCAST
        || matches!(
            v4.octets(),
            [192, 0, 2, _]      // TEST-NET-1  (RFC 5737)
            | [198, 51, 100, _] // TEST-NET-2
            | [203, 0, 113, _] // TEST-NET-3
        );
    if blocked {
        return Err(UrlGuardError::PrivateAddress(v4.to_string()));
    }
    Ok(())
}

/// Return `Err` for IPv6 addresses in loopback (::1), unspecified (::),
/// unique-local (fc00::/7, which includes fd00::/8 per RFC 4193), and
/// link-local (fe80::/10).
///
/// IPv4-mapped (`::ffff:a.b.c.d`) and IPv4-compatible (`::a.b.c.d`) literals are
/// held to the IPv4 rules first: otherwise a blocked v4 target — e.g. the cloud
/// instance-metadata endpoint `169.254.169.254` — could be smuggled through an
/// IPv6 literal and, on a dual-stack host that routes mapped v6 to v4, reach the
/// address the guard claims to reject. The delegation only *adds* rejections; the
/// native v6 checks below still own `::1`/`::` (which `to_ipv4()` renders as
/// `0.0.0.1`/`0.0.0.0`, not caught by the v4 rules).
fn check_ipv6_ssrf(v6: std::net::Ipv6Addr) -> Result<(), UrlGuardError> {
    if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
        check_ipv4_ssrf(v4)?;
    }
    let blocked = v6.is_loopback()
        || v6.is_unspecified()
        || (v6.segments()[0] & 0xfe00) == 0xfc00  // unique-local fc00::/7
        || (v6.segments()[0] & 0xffc0) == 0xfe80; // link-local   fe80::/10
    if blocked {
        return Err(UrlGuardError::PrivateAddress(format!("[{v6}]")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_guard_accepts_public_https_url() {
        assert!(check_public_url("https://example.com/hook").is_ok());
        assert!(check_public_url("https://hooks.example.com:8443/webhook").is_ok());
        assert!(check_public_url("http://198.51.101.1/hook").is_ok()); // public, not TEST-NET-2
    }

    #[test]
    fn ssrf_guard_rejects_localhost_by_name() {
        assert!(check_public_url("https://localhost/admin").is_err());
        assert!(check_public_url("http://LOCALHOST:8080/").is_err());
    }

    #[test]
    fn ssrf_guard_rejects_loopback_ip() {
        assert!(check_public_url("http://127.0.0.1/").is_err());
        assert!(check_public_url("http://127.255.255.254/").is_err());
        assert!(check_public_url("http://[::1]/").is_err());
    }

    #[test]
    fn ssrf_guard_rejects_link_local() {
        // AWS/GCP/Azure instance metadata endpoint.
        assert!(check_public_url("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(check_public_url("http://[fe80::1]/hook").is_err());
    }

    #[test]
    fn ssrf_guard_rejects_private_rfc1918() {
        assert!(check_public_url("http://10.0.0.1/hook").is_err());
        assert!(check_public_url("http://172.16.0.1/hook").is_err());
        assert!(check_public_url("http://192.168.1.100/hook").is_err());
    }

    #[test]
    fn ssrf_guard_rejects_documentation_blocks() {
        assert!(check_public_url("http://192.0.2.1/hook").is_err()); // TEST-NET-1
        assert!(check_public_url("http://198.51.100.1/hook").is_err()); // TEST-NET-2
        assert!(check_public_url("http://203.0.113.1/hook").is_err()); // TEST-NET-3
    }

    #[test]
    fn ssrf_guard_rejects_non_http_schemes() {
        assert!(check_public_url("ftp://example.com/hook").is_err());
        assert!(check_public_url("file:///etc/passwd").is_err());
        assert!(check_public_url("gopher://example.com/").is_err());
    }

    #[test]
    fn ssrf_guard_rejects_malformed_url() {
        assert!(check_public_url("not-a-url").is_err());
        assert!(check_public_url("").is_err());
    }

    #[test]
    fn ssrf_guard_rejects_unique_local_ipv6() {
        // fc00::/7 (unique-local, includes fd00::/8 used by RFC 4193).
        assert!(check_public_url("http://[fd12:3456:789a::1]/hook").is_err());
    }

    #[test]
    fn ssrf_guard_rejects_ipv4_mapped_and_compatible_ipv6() {
        // IPv4-mapped (::ffff:a.b.c.d): a blocked v4 target must not be
        // smuggled through an IPv6 literal. Cloud metadata + loopback + RFC1918.
        assert!(check_public_url("http://[::ffff:169.254.169.254]/latest/meta-data/").is_err());
        assert!(check_public_url("http://[::ffff:127.0.0.1]/").is_err());
        assert!(check_public_url("http://[::ffff:10.0.0.1]/hook").is_err());
        // IPv4-compatible (::a.b.c.d, deprecated but still routable on some hosts).
        assert!(check_public_url("http://[::169.254.169.254]/latest/meta-data/").is_err());
    }

    #[test]
    fn ssrf_guard_allows_public_ipv4_mapped_ipv6() {
        // A public address in IPv4-mapped form stays permitted; the delegation
        // only adds rejections, it must not block legitimate targets.
        assert!(check_public_url("http://[::ffff:93.184.216.34]/hook").is_ok());
    }
}
