// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! The one guarded HTTP client for every outbound request.
//!
//! [`client_for`] parses the target URL, resolves its host, holds every
//! resolved address to the [`Allowance`], and returns a client pinned to those
//! addresses. The request cannot reach a different address than the one that
//! was checked (DNS rebinding), cannot be redirected elsewhere (redirects are
//! never followed), and cannot hang (connect 10 s, total 15 s).

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use reqwest::redirect;
use url::{Host, Url};

use crate::url_guard::{UrlGuardError, check_public_ip, check_public_url};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = concat!("envelope/", env!("CARGO_PKG_VERSION"));

/// Which targets a request may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Allowance {
    /// Public internet only: every resolved address must pass
    /// [`check_public_ip`]. The default for any URL a user or sender supplied.
    Public,
    /// The operator's own configured service. Requests to exactly this host
    /// skip the address check (so a tailnet or localhost deployment works);
    /// any other host is held to [`Allowance::Public`].
    OperatorService(String),
    /// Loopback only: every resolved address must be a loopback address.
    Loopback,
}

/// Why an outbound request was refused before it was sent.
#[derive(Debug)]
pub enum EgressError {
    /// The URL or one of its resolved addresses failed the SSRF guard.
    Blocked(UrlGuardError),
    /// [`Allowance::Loopback`] was requested but the host resolved elsewhere.
    NotLoopback(String),
    /// The host could not be resolved, or resolved to nothing.
    Resolve(String),
    /// The HTTP client could not be built.
    Client(String),
}

impl std::fmt::Display for EgressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EgressError::Blocked(e) => write!(f, "{e}"),
            EgressError::NotLoopback(addr) => {
                write!(
                    f,
                    "loopback-only request resolved to non-loopback address {addr}"
                )
            }
            EgressError::Resolve(msg) => write!(f, "could not resolve host: {msg}"),
            EgressError::Client(msg) => write!(f, "could not build HTTP client: {msg}"),
        }
    }
}

impl std::error::Error for EgressError {}

impl From<UrlGuardError> for EgressError {
    fn from(e: UrlGuardError) -> Self {
        EgressError::Blocked(e)
    }
}

/// Build a client for one request to `raw_url`, returning it with the
/// validated URL to send to. Resolves the host through the system resolver.
pub async fn client_for(
    raw_url: &str,
    allowance: &Allowance,
) -> Result<(reqwest::Client, Url), EgressError> {
    client_for_with(raw_url, allowance, system_resolve).await
}

async fn system_resolve(host: String, port: u16) -> io::Result<Vec<IpAddr>> {
    let addrs = tokio::net::lookup_host((host.as_str(), port)).await?;
    Ok(addrs.map(|a| a.ip()).collect())
}

/// [`client_for`] with an injectable resolver, so tests never touch real DNS.
async fn client_for_with<R, F>(
    raw_url: &str,
    allowance: &Allowance,
    resolve: R,
) -> Result<(reqwest::Client, Url), EgressError>
where
    R: FnOnce(String, u16) -> F,
    F: Future<Output = io::Result<Vec<IpAddr>>>,
{
    let url = Url::parse(raw_url).map_err(|_| UrlGuardError::Malformed(raw_url.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(UrlGuardError::UnsupportedScheme(url.scheme().to_string()).into());
    }
    let host = url.host().ok_or(UrlGuardError::MissingHost)?.to_owned();

    let trusted = matches!(
        (allowance, &host),
        (Allowance::OperatorService(h), Host::Domain(name)) if name.eq_ignore_ascii_case(h)
    );
    let loopback_only = *allowance == Allowance::Loopback;
    if !trusted && !loopback_only {
        // Scheme, `localhost`, and literal private IPs are refused before any
        // lookup happens.
        check_public_url(raw_url)?;
    }

    let ips: Vec<IpAddr> = match &host {
        Host::Ipv4(v4) => vec![IpAddr::V4(*v4)],
        Host::Ipv6(v6) => vec![IpAddr::V6(*v6)],
        Host::Domain(name) => {
            let port = url.port_or_known_default().unwrap_or(0);
            resolve(name.clone(), port)
                .await
                .map_err(|e| EgressError::Resolve(format!("{name}: {e}")))?
        }
    };
    if ips.is_empty() {
        return Err(EgressError::Resolve(format!("{host}: no addresses")));
    }
    if !trusted {
        for ip in &ips {
            if loopback_only {
                if !is_loopback(*ip) {
                    return Err(EgressError::NotLoopback(ip.to_string()));
                }
            } else {
                check_public_ip(*ip)?;
            }
        }
    }

    let mut builder = reqwest::Client::builder()
        .redirect(redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .user_agent(USER_AGENT);
    if let Host::Domain(name) = &host {
        // Port 0 keeps the URL's own port; only the address is pinned.
        let pinned: Vec<SocketAddr> = ips.iter().map(|ip| SocketAddr::new(*ip, 0)).collect();
        builder = builder.resolve_to_addrs(name, &pinned);
    }
    if loopback_only {
        builder = builder.no_proxy();
    }
    let client = builder
        .build()
        .map_err(|e| EgressError::Client(e.to_string()))?;
    Ok((client, url))
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A resolver that answers every lookup with `ips` and never touches DNS.
    fn fixed(
        ips: Vec<IpAddr>,
    ) -> impl FnOnce(String, u16) -> std::future::Ready<io::Result<Vec<IpAddr>>> {
        move |_, _| std::future::ready(Ok(ips))
    }

    /// A resolver that fails the test if it is ever called.
    fn unreachable_resolver(_: String, _: u16) -> std::future::Ready<io::Result<Vec<IpAddr>>> {
        panic!("literal-IP and pre-rejected URLs must not be resolved")
    }

    async fn check(url: &str, allowance: &Allowance, ips: Vec<IpAddr>) -> Result<Url, EgressError> {
        client_for_with(url, allowance, fixed(ips))
            .await
            .map(|(_, u)| u)
    }

    #[tokio::test]
    async fn public_rejects_private_loopback_link_local_and_metadata_literals() {
        for url in [
            "http://10.0.0.1/hook",
            "http://192.168.1.10/hook",
            "http://127.0.0.1:8080/hook",
            "http://[::1]/hook",
            "http://169.254.169.254/latest/meta-data/",
            "http://[fe80::1]/hook",
            "http://[::ffff:169.254.169.254]/",
            "http://localhost/hook",
        ] {
            let err = client_for_with(url, &Allowance::Public, unreachable_resolver)
                .await
                .expect_err(url);
            assert!(matches!(err, EgressError::Blocked(_)), "{url}: {err}");
        }
    }

    #[tokio::test]
    async fn public_rejects_non_http_scheme() {
        let err = client_for_with(
            "file:///etc/passwd",
            &Allowance::Public,
            unreachable_resolver,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            EgressError::Blocked(UrlGuardError::UnsupportedScheme(_))
        ));
    }

    #[tokio::test]
    async fn public_host_resolving_to_private_address_is_rejected() {
        for ip in [
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ] {
            let err = check("https://hooks.example.com/x", &Allowance::Public, vec![ip])
                .await
                .expect_err(&ip.to_string());
            assert!(matches!(err, EgressError::Blocked(_)), "{ip}: {err}");
        }
    }

    #[tokio::test]
    async fn public_host_is_rejected_if_any_resolved_address_is_private() {
        let err = check(
            "https://hooks.example.com/x",
            &Allowance::Public,
            vec![
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            ],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, EgressError::Blocked(_)), "{err}");
    }

    #[tokio::test]
    async fn public_host_resolving_to_public_address_is_allowed() {
        let url = check(
            "https://hooks.example.com/x?a=1",
            &Allowance::Public,
            vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
        )
        .await
        .unwrap();
        assert_eq!(url.as_str(), "https://hooks.example.com/x?a=1");
    }

    #[tokio::test]
    async fn host_resolving_to_nothing_is_rejected() {
        let err = check("https://hooks.example.com/x", &Allowance::Public, vec![])
            .await
            .unwrap_err();
        assert!(matches!(err, EgressError::Resolve(_)), "{err}");
    }

    #[tokio::test]
    async fn loopback_allowed_only_under_loopback_allowance() {
        let loopback = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
        assert!(
            check("http://127.0.0.1:9/hook", &Allowance::Loopback, vec![])
                .await
                .is_ok()
        );
        assert!(
            check("http://[::1]:9/hook", &Allowance::Loopback, vec![])
                .await
                .is_ok()
        );
        assert!(
            check(
                "http://localhost:9/hook",
                &Allowance::Loopback,
                loopback.clone()
            )
            .await
            .is_ok()
        );

        assert!(
            check("http://127.0.0.1:9/hook", &Allowance::Public, vec![])
                .await
                .is_err()
        );
        assert!(
            check("http://localhost:9/hook", &Allowance::Public, loopback)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn loopback_allowance_rejects_non_loopback_targets() {
        for (url, ips) in [
            ("http://10.0.0.1/hook", vec![]),
            ("http://93.184.216.34/hook", vec![]),
            (
                "http://localhost/hook",
                vec![
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                ],
            ),
        ] {
            let err = check(url, &Allowance::Loopback, ips).await.expect_err(url);
            assert!(matches!(err, EgressError::NotLoopback(_)), "{url}: {err}");
        }
    }

    #[tokio::test]
    async fn operator_service_allows_only_its_own_host() {
        let op = Allowance::OperatorService("cairn.tail1234.ts.net".to_string());
        let tailnet = vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 7))];
        let private = vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))];
        assert!(
            check("https://cairn.tail1234.ts.net/pull", &op, tailnet)
                .await
                .is_ok()
        );
        assert!(
            check("https://Cairn.Tail1234.ts.net/pull", &op, private.clone())
                .await
                .is_ok()
        );

        // Any other host is held to the public rules.
        assert!(
            check("https://evil.example.com/x", &op, private)
                .await
                .is_err()
        );
        assert!(check("http://169.254.169.254/", &op, vec![]).await.is_err());
        assert!(
            check(
                "http://localhost/",
                &op,
                vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
            )
            .await
            .is_err()
        );
    }

    /// Accept connections forever, answering each request with `response` and
    /// counting how many requests arrived.
    async fn serve(response: String) -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, hits)
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let (target, target_hits) =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()).await;
        let (origin, origin_hits) = serve(format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{target}/internal\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        ))
        .await;

        let (client, url) = client_for_with(
            &format!("http://{origin}/hook"),
            &Allowance::Loopback,
            unreachable_resolver,
        )
        .await
        .unwrap();
        let resp = client.post(url).body("x").send().await.unwrap();

        assert_eq!(resp.status().as_u16(), 302);
        assert_eq!(origin_hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            target_hits.load(Ordering::SeqCst),
            0,
            "the redirect target must never receive a request"
        );
    }

    #[tokio::test]
    async fn request_goes_to_the_pinned_address() {
        // A made-up hostname only reaches the local server because the client
        // is pinned to the address the resolver returned.
        let (origin, hits) =
            serve("HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into()).await;
        let (client, url) = client_for_with(
            &format!("http://pinned.invalid:{}/hook", origin.port()),
            &Allowance::Loopback,
            fixed(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        )
        .await
        .unwrap();
        let resp = client.post(url).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 204);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
