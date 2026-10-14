// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! TLS for every server connection an account makes.
//!
//! An account trusts the public certificate authorities in `webpki-roots`
//! unless it pins one certificate (`envelope accounts add --trust-cert`).
//! A pinned account accepts a server only when it presents exactly that
//! certificate, byte for byte, and signs the handshake with that
//! certificate's key. The pin replaces the public roots for that account and
//! never adds to them; nothing here accepts a server it has not verified.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use envelope_email_store::server_tls::sha256_fingerprint;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// A TLS handshake that did not complete.
#[derive(Debug)]
pub(crate) struct TlsFailure {
    pub(crate) message: String,
    /// The server presented a certificate other than the pinned one. Trying
    /// again will not help until the user trusts a new certificate.
    pub(crate) pin_mismatch: bool,
}

/// Run the TLS handshake on `tcp`. With `pinned_cert` the server must present
/// exactly that certificate; without it, a certificate that chains to the
/// public roots and names `host`. `account` is the address the re-trust hint
/// names.
pub(crate) async fn handshake(
    tcp: TcpStream,
    host: &str,
    port: u16,
    pinned_cert: Option<&[u8]>,
    account: &str,
) -> Result<TlsStream<TcpStream>, TlsFailure> {
    let server_name = ServerName::try_from(host)
        .map_err(|e| TlsFailure {
            message: format!("invalid server name {host}: {e}"),
            pin_mismatch: false,
        })?
        .to_owned();
    let handshake_failed = |e: std::io::Error| TlsFailure {
        message: format!("TLS handshake with {host}: {e}"),
        pin_mismatch: false,
    };

    let Some(pinned_cert) = pinned_cert else {
        let connector = TlsConnector::from(Arc::new(webpki_client_config()));
        return connector
            .connect(server_name, tcp)
            .await
            .map_err(handshake_failed);
    };

    let verifier = Arc::new(PinnedCertVerifier {
        pinned: CertificateDer::from(pinned_cert.to_vec()),
        provider: Arc::new(rustls::crypto::ring::default_provider()),
        refused: OnceLock::new(),
    });
    let config = ClientConfig::builder_with_provider(verifier.provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| TlsFailure {
            message: format!("TLS setup for {host}: {e}"),
            pin_mismatch: false,
        })?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    let result = TlsConnector::from(Arc::new(config))
        .connect(server_name, tcp)
        .await;
    match (result, verifier.refused.get()) {
        (Ok(stream), _) => Ok(stream),
        (Err(_), Some(presented)) => Err(TlsFailure {
            message: format!(
                "{host}:{port} presented a certificate that is not the one trusted for {account} \
                 (trusted SHA-256 {}, presented SHA-256 {presented}), so Envelope did not sign in. If the \
                 server's certificate changed on purpose, for example a new Proton Mail Bridge \
                 certificate, export it and trust it with: \
                 envelope accounts tls --account {account} --trust-cert <file.pem>",
                sha256_fingerprint(pinned_cert)
            ),
            pin_mismatch: true,
        }),
        (Err(e), None) => Err(handshake_failed(e)),
    }
}

/// The public certificate authorities, as every unpinned account uses.
fn webpki_client_config() -> ClientConfig {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth()
}

/// Accepts exactly one certificate. The pin is the server's identity, the
/// way an SSH host key is, so the names and dates inside the certificate are
/// not checked: Proton Mail Bridge's certificate is a CA certificate naming
/// 127.0.0.1, which WebPKI path building refuses as a server certificate
/// (`CaUsedAsEndEntity`). The handshake signature is always verified, against
/// the pinned certificate's key.
#[derive(Debug)]
struct PinnedCertVerifier {
    pinned: CertificateDer<'static>,
    provider: Arc<CryptoProvider>,
    /// SHA-256 of a certificate this verifier refused, for the error.
    refused: OnceLock<String>,
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.pinned.as_ref() {
            return Ok(ServerCertVerified::assertion());
        }
        let _ = self.refused.set(sha256_fingerprint(end_entity));
        Err(rustls::Error::InvalidCertificate(
            CertificateError::ApplicationVerificationFailure,
        ))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        _cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            &self.pinned,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        _cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            &self.pinned,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The one certificate in a PEM file, as DER, for `--trust-cert`. Other PEM
/// sections, such as the private key Proton Mail Bridge exports alongside
/// its certificate, are skipped and never kept.
pub fn read_pem_certificate(path: &Path) -> Result<Vec<u8>, String> {
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{} is not a readable PEM file: {e}", path.display()))?;
    match certs.as_slice() {
        [cert] => Ok(cert.to_vec()),
        [] => Err(format!(
            "{} has no certificate (no BEGIN CERTIFICATE section)",
            path.display()
        )),
        more => Err(format!(
            "{} has {} certificates; pass a file with only the server's certificate",
            path.display(),
            more.len()
        )),
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    //! Loopback IMAP and SMTP servers that present a caller-supplied
    //! certificate, for tests of certificate pinning and STARTTLS. Each
    //! serves one connection and records every line the client sent, marked
    //! with whether TLS was up at the time.

    use std::sync::{Arc, Mutex};

    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    /// A server certificate and its PKCS#8 private key, both DER.
    #[derive(Clone)]
    pub struct ServerCert {
        pub cert_der: Vec<u8>,
        pub key_der: Vec<u8>,
    }

    /// How the server starts TLS.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Mode {
        /// TLS from the first byte.
        ImplicitTls,
        /// Plain greeting; STARTTLS offered.
        Starttls,
        /// Plain greeting; STARTTLS never offered.
        PlainOnly,
    }

    #[derive(Clone, Default)]
    pub struct Transcript(Arc<Mutex<Vec<(bool, String)>>>);

    impl Transcript {
        fn push(&self, tls: bool, line: &str) {
            self.0
                .lock()
                .unwrap()
                .push((tls, line.trim_end().to_string()));
        }

        /// Command verbs the client sent before TLS, in order.
        pub fn plaintext_verbs(&self) -> Vec<String> {
            self.verbs(false)
        }

        /// Command verbs the client sent over TLS, in order.
        pub fn tls_verbs(&self) -> Vec<String> {
            self.verbs(true)
        }

        fn verbs(&self, tls: bool) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|(t, _)| *t == tls)
                .map(|(_, line)| verb(line))
                .collect()
        }
    }

    /// The command word: the second token for IMAP (after the tag), the
    /// first for SMTP.
    fn verb(line: &str) -> String {
        let mut words = line.split_whitespace();
        let first = words.next().unwrap_or_default().to_ascii_uppercase();
        let is_smtp = [
            "EHLO", "HELO", "STARTTLS", "AUTH", "MAIL", "RCPT", "DATA", "QUIT",
        ]
        .contains(&first.as_str());
        if is_smtp {
            first
        } else {
            words.next().unwrap_or_default().to_ascii_uppercase()
        }
    }

    fn acceptor(cert: &ServerCert) -> TlsAcceptor {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert.cert_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_der.clone())),
            )
            .unwrap();
        TlsAcceptor::from(Arc::new(config))
    }

    async fn listen() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    pub struct FakeServer {
        pub port: u16,
        pub transcript: Transcript,
    }

    /// An IMAP server that accepts any LOGIN, but only over TLS.
    pub async fn imap(mode: Mode, cert: ServerCert) -> FakeServer {
        let (listener, port) = listen().await;
        let transcript = Transcript::default();
        let record = transcript.clone();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let acceptor = acceptor(&cert);
            if mode == Mode::ImplicitTls {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    imap_session(BufReader::new(tls), true, mode, &record).await;
                }
                return;
            }
            let mut plain = BufReader::new(tcp);
            if !imap_session(&mut plain, false, mode, &record).await {
                return;
            }
            if let Ok(tls) = acceptor.accept(plain.into_inner()).await {
                imap_session(BufReader::new(tls), true, mode, &record).await;
            }
        });
        FakeServer { port, transcript }
    }

    /// Serves IMAP commands until the client leaves. Returns true when the
    /// client asked for STARTTLS and the server agreed.
    async fn imap_session<S>(mut stream: S, tls: bool, mode: Mode, record: &Transcript) -> bool
    where
        S: AsyncBufRead + AsyncWrite + Unpin,
    {
        let greet = mode == Mode::ImplicitTls || !tls;
        if greet && stream.write_all(b"* OK fake IMAP ready\r\n").await.is_err() {
            return false;
        }
        loop {
            let mut line = String::new();
            if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                return false;
            }
            record.push(tls, &line);
            let tag = line.split_whitespace().next().unwrap_or("*").to_string();
            let reply = match verb(&line).as_str() {
                "CAPABILITY" => {
                    let caps = if !tls && mode == Mode::Starttls {
                        "IMAP4rev1 STARTTLS LOGINDISABLED"
                    } else if !tls {
                        "IMAP4rev1 LOGINDISABLED"
                    } else {
                        "IMAP4rev1 AUTH=PLAIN"
                    };
                    format!("* CAPABILITY {caps}\r\n{tag} OK CAPABILITY completed\r\n")
                }
                "STARTTLS" if !tls && mode == Mode::Starttls => {
                    let _ = stream
                        .write_all(format!("{tag} OK Begin TLS now\r\n").as_bytes())
                        .await;
                    return true;
                }
                "LOGIN" if tls => format!("{tag} OK LOGIN completed\r\n"),
                "LOGIN" => format!("{tag} NO [PRIVACYREQUIRED] LOGIN refused\r\n"),
                "LOGOUT" => format!("* BYE\r\n{tag} OK LOGOUT completed\r\n"),
                _ => format!("{tag} BAD unknown command\r\n"),
            };
            if stream.write_all(reply.as_bytes()).await.is_err() {
                return false;
            }
        }
    }

    /// An SMTP submission server that accepts AUTH and any message, but
    /// only over TLS.
    pub async fn smtp(mode: Mode, cert: ServerCert) -> FakeServer {
        let (listener, port) = listen().await;
        let transcript = Transcript::default();
        let record = transcript.clone();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let acceptor = acceptor(&cert);
            if mode == Mode::ImplicitTls {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    smtp_session(BufReader::new(tls), true, mode, &record).await;
                }
                return;
            }
            let mut plain = BufReader::new(tcp);
            if !smtp_session(&mut plain, false, mode, &record).await {
                return;
            }
            if let Ok(tls) = acceptor.accept(plain.into_inner()).await {
                smtp_session(BufReader::new(tls), true, mode, &record).await;
            }
        });
        FakeServer { port, transcript }
    }

    /// Serves SMTP commands until the client leaves. Returns true when the
    /// client asked for STARTTLS and the server agreed.
    async fn smtp_session<S>(mut stream: S, tls: bool, mode: Mode, record: &Transcript) -> bool
    where
        S: AsyncBufRead + AsyncWrite + Unpin,
    {
        let greet = mode == Mode::ImplicitTls || !tls;
        if greet && stream.write_all(b"220 fake ESMTP\r\n").await.is_err() {
            return false;
        }
        loop {
            let mut line = String::new();
            if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                return false;
            }
            record.push(tls, &line);
            let reply = match verb(&line).as_str() {
                "EHLO" if tls => "250-fake\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n",
                "EHLO" if mode == Mode::Starttls => "250-fake\r\n250-STARTTLS\r\n250 8BITMIME\r\n",
                "EHLO" => "250-fake\r\n250 8BITMIME\r\n",
                "STARTTLS" if !tls && mode == Mode::Starttls => {
                    let _ = stream.write_all(b"220 2.0.0 Ready to start TLS\r\n").await;
                    return true;
                }
                "AUTH" if tls => "235 2.7.0 Authentication successful\r\n",
                "MAIL" | "RCPT" if tls => "250 2.1.0 Ok\r\n",
                "DATA" if tls => {
                    if stream.write_all(b"354 End data with .\r\n").await.is_err() {
                        return false;
                    }
                    loop {
                        let mut body = String::new();
                        if stream.read_line(&mut body).await.unwrap_or(0) == 0 {
                            return false;
                        }
                        if body == ".\r\n" {
                            break;
                        }
                    }
                    "250 2.0.0 Ok: queued as FAKE1\r\n"
                }
                "QUIT" => {
                    let _ = stream.write_all(b"221 2.0.0 Bye\r\n").await;
                    return false;
                }
                _ => "530 5.7.0 Must issue a STARTTLS command first\r\n",
            };
            if stream.write_all(reply.as_bytes()).await.is_err() {
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeServer, Mode, ServerCert};
    use envelope_email_store::{Account, AccountWithCredentials, ImapSecurity, ServerTls};
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, SanType};

    /// A certificate shaped like Proton Mail Bridge's: self-signed, a CA
    /// certificate, naming 127.0.0.1 by IP address. (Bridge uses RSA 2048;
    /// rcgen's ring backend makes ECDSA P-256 keys.)
    fn bridge_like_cert() -> ServerCert {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "127.0.0.1");
        params.subject_alt_names = vec![SanType::IpAddress([127, 0, 0, 1].into())];
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        ServerCert {
            cert_der: cert.der().to_vec(),
            key_der: key.serialize_der(),
        }
    }

    fn account(
        imap_port: u16,
        smtp_port: u16,
        imap_security: Option<ImapSecurity>,
        pinned_cert: Option<&ServerCert>,
    ) -> AccountWithCredentials {
        AccountWithCredentials {
            account: Account {
                id: "acc-bridge".into(),
                name: "Bridge".into(),
                username: "you@proton.me".into(),
                domain: "proton.me".into(),
                smtp_host: "127.0.0.1".into(),
                smtp_port,
                imap_host: "127.0.0.1".into(),
                imap_port,
                smtp_username: None,
                imap_username: None,
                display_name: None,
                signature_text: None,
                signature_html: None,
                created_at: "2026-10-14T00:00:00Z".into(),
            },
            password: "bridge-password".into(),
            smtp_password: None,
            imap_password: None,
            oauth: None,
            server_tls: ServerTls {
                imap_security,
                pinned_cert: pinned_cert.map(|c| c.cert_der.clone()),
            },
        }
    }

    fn install_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    async fn imap_connect(
        server: &FakeServer,
        security: ImapSecurity,
        pin: Option<&ServerCert>,
    ) -> Result<(), String> {
        install_provider();
        let creds = account(server.port, 9, Some(security), pin);
        crate::imap::connect(&creds)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn fingerprint(cert: &ServerCert) -> String {
        envelope_email_store::server_tls::sha256_fingerprint(&cert.cert_der)
    }

    #[tokio::test]
    async fn a_pinned_certificate_is_accepted_over_implicit_tls() {
        let cert = bridge_like_cert();
        let server = super::testing::imap(Mode::ImplicitTls, cert.clone()).await;

        let result = imap_connect(&server, ImapSecurity::Tls, Some(&cert)).await;

        assert_eq!(result, Ok(()));
        assert_eq!(server.transcript.tls_verbs(), vec!["LOGIN"]);
    }

    #[tokio::test]
    async fn a_different_self_signed_certificate_is_refused_naming_both_fingerprints() {
        let pinned = bridge_like_cert();
        let presented = bridge_like_cert();
        let server = super::testing::imap(Mode::ImplicitTls, presented.clone()).await;

        let err = imap_connect(&server, ImapSecurity::Tls, Some(&pinned))
            .await
            .unwrap_err();

        assert!(err.contains(&format!("127.0.0.1:{}", server.port)), "{err}");
        assert!(err.contains(&fingerprint(&pinned)), "{err}");
        assert!(err.contains(&fingerprint(&presented)), "{err}");
        assert!(
            err.contains("envelope accounts tls --account you@proton.me --trust-cert"),
            "{err}"
        );
        assert!(server.transcript.tls_verbs().is_empty());
    }

    #[tokio::test]
    async fn without_a_pin_a_self_signed_certificate_is_refused_as_before() {
        let cert = bridge_like_cert();
        let server = super::testing::imap(Mode::ImplicitTls, cert).await;

        let err = imap_connect(&server, ImapSecurity::Tls, None)
            .await
            .unwrap_err();

        assert!(err.contains("TLS handshake with 127.0.0.1"), "{err}");
        assert!(err.contains("invalid peer certificate"), "{err}");
        assert!(server.transcript.tls_verbs().is_empty());
    }

    #[tokio::test]
    async fn starttls_logs_in_only_after_the_upgrade() {
        let cert = bridge_like_cert();
        let server = super::testing::imap(Mode::Starttls, cert.clone()).await;

        let result = imap_connect(&server, ImapSecurity::Starttls, Some(&cert)).await;

        assert_eq!(result, Ok(()));
        assert_eq!(
            server.transcript.plaintext_verbs(),
            vec!["CAPABILITY", "STARTTLS"]
        );
        assert_eq!(server.transcript.tls_verbs(), vec!["LOGIN"]);
    }

    #[tokio::test]
    async fn a_server_without_starttls_is_refused_before_login() {
        let cert = bridge_like_cert();
        let server = super::testing::imap(Mode::PlainOnly, cert.clone()).await;

        let err = imap_connect(&server, ImapSecurity::Starttls, Some(&cert))
            .await
            .unwrap_err();

        assert!(err.contains("does not offer STARTTLS"), "{err}");
        assert_eq!(server.transcript.plaintext_verbs(), vec!["CAPABILITY"]);
        assert!(server.transcript.tls_verbs().is_empty());
    }

    #[tokio::test]
    async fn starttls_without_a_pin_still_requires_a_publicly_trusted_certificate() {
        let cert = bridge_like_cert();
        let server = super::testing::imap(Mode::Starttls, cert).await;

        let err = imap_connect(&server, ImapSecurity::Starttls, None)
            .await
            .unwrap_err();

        assert!(err.contains("invalid peer certificate"), "{err}");
        assert!(
            !server
                .transcript
                .plaintext_verbs()
                .contains(&"LOGIN".to_string())
        );
        assert!(server.transcript.tls_verbs().is_empty());
    }

    #[tokio::test]
    async fn idle_sessions_use_the_same_pin_and_starttls() {
        install_provider();
        let cert = bridge_like_cert();
        let server = super::testing::imap(Mode::Starttls, cert.clone()).await;
        let creds = account(server.port, 9, Some(ImapSecurity::Starttls), Some(&cert));

        let session = crate::idle::connect_session(&creds).await;

        assert!(session.is_ok(), "{:?}", session.err());
        assert_eq!(server.transcript.tls_verbs(), vec!["LOGIN"]);
    }

    #[tokio::test]
    async fn data_sent_between_starttls_and_tls_is_refused() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(tcp);
            let mut line = String::new();
            stream.write_all(b"* OK ready\r\n").await.unwrap();
            stream.read_line(&mut line).await.unwrap();
            stream
                .write_all(b"* CAPABILITY IMAP4rev1 STARTTLS\r\nS1 OK done\r\n")
                .await
                .unwrap();
            stream.read_line(&mut line).await.unwrap();
            // One write: the agreement and an injected response together.
            stream
                .write_all(b"S2 OK Begin TLS\r\nA0001 OK LOGIN completed\r\n")
                .await
                .unwrap();
            let _ = stream.read_line(&mut line).await;
        });
        let cert = bridge_like_cert();
        let creds = account(port, 9, Some(ImapSecurity::Starttls), Some(&cert));

        let err = crate::imap::connect(&creds)
            .await
            .err()
            .unwrap()
            .to_string();

        assert!(
            err.contains("sent data after agreeing to STARTTLS"),
            "{err}"
        );
    }

    /// A server that accepts the connection and never sends a byte.
    async fn silent_server() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((tcp, _)) = listener.accept().await {
                held.push(tcp);
            }
        });
        port
    }

    /// Open a session with a short pre-login limit, failing the test if it
    /// is still waiting after five seconds.
    async fn open_with_short_limit(creds: &AccountWithCredentials) -> String {
        install_provider();
        let limit = std::time::Duration::from_millis(300);
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::imap::open_session_within(creds, limit),
        )
        .await
        {
            Ok(Ok(_)) => panic!("connected to a server that never set up TLS"),
            Ok(Err(e)) => e.to_string(),
            Err(_) => panic!("IMAP connect still waiting after 5s"),
        }
    }

    #[tokio::test]
    async fn starttls_against_a_silent_server_gives_up_and_suggests_tls() {
        let port = silent_server().await;
        let creds = account(port, 9, Some(ImapSecurity::Starttls), None);

        let err = open_with_short_limit(&creds).await;

        assert!(err.contains(&format!("127.0.0.1:{port}")), "{err}");
        assert!(err.contains("--imap-security tls"), "{err}");
    }

    #[tokio::test]
    async fn starttls_against_an_implicit_tls_server_gives_up_and_suggests_tls() {
        // Proton Mail Bridge with its IMAP connection mode switched to SSL:
        // it waits for a TLS ClientHello while Envelope waits for "* OK".
        let cert = bridge_like_cert();
        let server = super::testing::imap(Mode::ImplicitTls, cert.clone()).await;
        let creds = account(server.port, 9, Some(ImapSecurity::Starttls), Some(&cert));

        let err = open_with_short_limit(&creds).await;

        assert!(err.contains("--imap-security tls"), "{err}");
        assert!(
            err.contains("envelope accounts tls --account you@proton.me --imap-security tls"),
            "{err}"
        );
        assert!(server.transcript.tls_verbs().is_empty());
    }

    #[tokio::test]
    async fn implicit_tls_against_a_silent_server_gives_up_and_suggests_starttls() {
        let port = silent_server().await;
        let creds = account(port, 9, Some(ImapSecurity::Tls), None);

        let err = open_with_short_limit(&creds).await;

        assert!(err.contains(&format!("127.0.0.1:{port}")), "{err}");
        assert!(err.contains("--imap-security starttls"), "{err}");
    }

    #[test]
    fn trust_cert_reads_the_certificate_and_never_the_key() {
        let mut params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        params.is_ca = IsCa::NoCa;
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let dir = tempfile::tempdir().unwrap();

        // Proton Mail Bridge exports cert.pem and key.pem; a combined file
        // must still yield only the certificate.
        let both = dir.path().join("both.pem");
        std::fs::write(&both, format!("{}{}", key.serialize_pem(), cert.pem())).unwrap();
        assert_eq!(
            super::read_pem_certificate(&both).unwrap(),
            cert.der().to_vec()
        );

        let key_only = dir.path().join("key.pem");
        std::fs::write(&key_only, key.serialize_pem()).unwrap();
        let err = super::read_pem_certificate(&key_only).unwrap_err();
        assert!(err.contains("has no certificate"), "{err}");

        let two = dir.path().join("two.pem");
        std::fs::write(&two, format!("{}{}", cert.pem(), cert.pem())).unwrap();
        let err = super::read_pem_certificate(&two).unwrap_err();
        assert!(err.contains("has 2 certificates"), "{err}");
    }

    fn message() -> lettre::Message {
        lettre::Message::builder()
            .from("you@proton.me".parse().unwrap())
            .to("friend@example.test".parse().unwrap())
            .subject("hello")
            .body("hi".to_string())
            .unwrap()
    }

    #[tokio::test]
    async fn smtp_starttls_with_a_pinned_certificate_submits() {
        install_provider();
        let cert = bridge_like_cert();
        let server = super::testing::smtp(Mode::Starttls, cert.clone()).await;
        let creds = account(9, server.port, None, Some(&cert));

        let reply = crate::smtp_submit::submit_once(
            &crate::smtp_submit::AccountConnector::new(&creds),
            &message(),
            crate::smtp_submit::Deadlines::default(),
        )
        .await;

        assert_eq!(
            reply.as_deref().map_err(|e| e.to_string()),
            Ok("250 2.0.0 Ok: queued as FAKE1")
        );
        assert_eq!(
            server.transcript.plaintext_verbs(),
            vec!["EHLO", "STARTTLS"]
        );
        assert_eq!(
            server.transcript.tls_verbs(),
            vec!["EHLO", "AUTH", "MAIL", "RCPT", "DATA", "QUIT"]
        );
    }

    #[tokio::test]
    async fn smtp_refuses_a_different_certificate_permanently() {
        use crate::smtp_submit::SmtpConnect;
        install_provider();
        let pinned = bridge_like_cert();
        let presented = bridge_like_cert();
        let server = super::testing::smtp(Mode::Starttls, presented.clone()).await;
        let creds = account(9, server.port, None, Some(&pinned));

        let failure = crate::smtp_submit::AccountConnector::new(&creds)
            .connect()
            .await
            .err()
            .unwrap();

        assert!(failure.permanent);
        assert!(
            failure.error.contains(&fingerprint(&pinned)),
            "{}",
            failure.error
        );
        assert!(
            failure.error.contains(&fingerprint(&presented)),
            "{}",
            failure.error
        );
        assert!(server.transcript.tls_verbs().is_empty());
    }

    #[tokio::test]
    async fn smtp_implicit_tls_with_a_pinned_certificate_connects() {
        let cert = bridge_like_cert();
        let server = super::testing::smtp(Mode::ImplicitTls, cert.clone()).await;

        let mut conn = crate::smtp_submit::connect_pinned(
            "127.0.0.1",
            server.port,
            true,
            &cert.cert_der,
            "you@proton.me",
            &lettre::transport::smtp::extension::ClientId::default(),
        )
        .await
        .map_err(|f| f.error)
        .unwrap();
        conn.quit().await.unwrap();

        assert!(server.transcript.plaintext_verbs().is_empty());
        assert_eq!(server.transcript.tls_verbs(), vec!["EHLO", "QUIT"]);
    }

    #[tokio::test]
    async fn smtp_without_starttls_is_refused_before_auth() {
        use crate::smtp_submit::SmtpConnect;
        install_provider();
        let cert = bridge_like_cert();
        let server = super::testing::smtp(Mode::PlainOnly, cert.clone()).await;
        let creds = account(9, server.port, None, Some(&cert));

        let failure = crate::smtp_submit::AccountConnector::new(&creds)
            .connect()
            .await
            .err()
            .unwrap();

        assert!(
            failure.error.contains("does not offer STARTTLS"),
            "{}",
            failure.error
        );
        assert_eq!(server.transcript.plaintext_verbs(), vec!["EHLO"]);
        assert!(server.transcript.tls_verbs().is_empty());
    }
}
