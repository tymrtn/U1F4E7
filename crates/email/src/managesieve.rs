// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! ManageSieve (RFC 5804) publishing for Envelope rules.
//!
//! This module connects to a ManageSieve server, authenticates with SASL
//! PLAIN over TLS, and uploads the exact script produced by
//! [`crate::sieve::export_sieve`] before activating it. It is **not** a
//! general ManageSieve library: it only implements the subset Envelope
//! needs to publish a single named script and activate it.
//!
//! Safety invariants:
//! - Default behavior at the CLI layer is dry-run; no network upload happens
//!   without explicit `--confirm`. This module exposes a pure
//!   [`build_plan`] helper for that dry-run JSON path.
//! - Passwords and SASL credentials are never logged. The protocol
//!   transcript is not captured anywhere by default.
//! - We only emit `PUTSCRIPT` and `SETACTIVE` for the script the operator
//!   asked for. We never `DELETESCRIPT` other scripts on the server.
//! - The first capability exchange is plaintext (per RFC 5804). We always
//!   `STARTTLS` before issuing `AUTHENTICATE` — credentials never leave
//!   the client unencrypted.

use std::sync::Arc;
use std::time::Duration;

use envelope_email_store::models::AccountWithCredentials;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio::time;
use tokio_rustls::TlsConnector;

/// Migadu's canonical IMAP host. Used to recognize Migadu accounts when
/// resolving ManageSieve endpoint defaults.
pub const MIGADU_IMAP_HOST: &str = "imap.migadu.com";

/// Migadu's ManageSieve host. Default for accounts whose IMAP host matches
/// [`MIGADU_IMAP_HOST`].
pub const MIGADU_SIEVE_HOST: &str = "sieve.migadu.com";

/// Standard ManageSieve port from RFC 5804.
pub const DEFAULT_SIEVE_PORT: u16 = 4190;

/// Errors raised by ManageSieve publishing.
#[derive(Debug, Error)]
pub enum ManageSieveError {
    /// TCP connect / TLS handshake / I/O failure.
    #[error("ManageSieve connection failed: {0}")]
    Connection(String),

    /// Protocol/parse error (unexpected response, malformed capabilities).
    #[error("ManageSieve protocol error: {0}")]
    Protocol(String),

    /// Server returned a hard NO/BYE response. The reason text never
    /// contains credentials.
    #[error("ManageSieve server refused command: {0}")]
    Refused(String),

    /// Authentication explicitly refused.
    #[error("ManageSieve authentication failed")]
    Auth,

    /// Local capability mismatch — the server cannot speak STARTTLS or
    /// PLAIN. Surfaced as a stable JSON code so operators know to switch
    /// hosts/ports rather than re-try.
    #[error("ManageSieve capability unavailable: {0}")]
    CapabilityUnavailable(String),
}

/// Resolve the ManageSieve endpoint to publish to.
///
/// Resolution priority:
/// 1. Explicit `override_host` / `override_port` (e.g. `--host`, `--port`).
/// 2. Provider default for the account's IMAP host (only Migadu today).
/// 3. The IMAP host with [`DEFAULT_SIEVE_PORT`] — a best-effort guess that
///    matches Dovecot/Pigeonhole deployments and the RFC 5804 default port.
///
/// Override resolution is field-by-field so an operator can override only
/// the port (e.g. `--port 4191`) without losing the provider default host.
pub fn resolve_sieve_endpoint(
    imap_host: &str,
    override_host: Option<&str>,
    override_port: Option<u16>,
) -> (String, u16) {
    let (default_host, default_port) = match migadu_defaults(imap_host) {
        Some(d) => d,
        None => (imap_host.to_string(), DEFAULT_SIEVE_PORT),
    };
    let host = override_host.map(|h| h.to_string()).unwrap_or(default_host);
    let port = override_port.unwrap_or(default_port);
    (host, port)
}

/// Provider-default ManageSieve endpoint for a known IMAP host.
///
/// Returns the canonical Migadu ManageSieve host/port when the IMAP host
/// is `imap.migadu.com` (case-insensitive). Returns `None` for everything
/// else.
pub fn migadu_defaults(imap_host: &str) -> Option<(String, u16)> {
    if imap_host.eq_ignore_ascii_case(MIGADU_IMAP_HOST) {
        Some((MIGADU_SIEVE_HOST.to_string(), DEFAULT_SIEVE_PORT))
    } else {
        None
    }
}

/// Format a ManageSieve quoted string per RFC 5804 §1.2. The result is
/// wrapped in `"..."` with `\` and `"` escaped. Use this for short fields
/// such as script names and SASL mechanism names.
///
/// Returns `None` when the value contains a literal CR or LF — those
/// require the literal form rather than a quoted string.
pub fn sieve_quoted(value: &str) -> Option<String> {
    if value.contains('\r') || value.contains('\n') {
        return None;
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    Some(format!("\"{escaped}\""))
}

/// Format a ManageSieve non-synchronizing literal (`{N+}\r\n<payload>`).
/// We use this for the script bytes because the script can be large and
/// contain quotes/backslashes.
///
/// Dovecot/Pigeonhole (Migadu's server) advertises the non-synchronizing
/// literal extension; using `{N+}` removes one round-trip and avoids the
/// `+ go ahead` continuation handshake.
pub fn sieve_literal(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 16);
    out.extend_from_slice(format!("{{{}+}}\r\n", payload.len()).as_bytes());
    out.extend_from_slice(payload);
    out
}

/// Encode SASL PLAIN credentials per RFC 4616: `\0<authcid>\0<password>`
/// base64-encoded. Authzid is empty.
pub fn sasl_plain_initial_response(authcid: &str, password: &str) -> String {
    use base64::Engine;
    let mut buf = Vec::with_capacity(authcid.len() + password.len() + 2);
    buf.push(0);
    buf.extend_from_slice(authcid.as_bytes());
    buf.push(0);
    buf.extend_from_slice(password.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(buf)
}

/// Capability snapshot parsed from the server's initial banner (or the
/// post-`STARTTLS` re-issued banner). Only fields Envelope needs.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub implementation: Option<String>,
    pub sieve_extensions: Vec<String>,
    pub sasl_mechanisms: Vec<String>,
    pub starttls: bool,
    pub version: Option<String>,
}

impl Capabilities {
    pub fn supports_sasl(&self, mechanism: &str) -> bool {
        self.sasl_mechanisms
            .iter()
            .any(|m| m.eq_ignore_ascii_case(mechanism))
    }
}

/// Status of a single ManageSieve response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseStatus {
    Ok,
    No,
    Bye,
}

/// Parse the leading word of a final response line (`OK`, `NO`, `BYE`).
/// The remainder of the line (response code + human text) is returned as
/// `reason` with surrounding whitespace trimmed.
pub fn classify_response(line: &str) -> Option<(ResponseStatus, String)> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if let Some(rest) = trimmed.strip_prefix("OK") {
        return Some((ResponseStatus::Ok, rest.trim().to_string()));
    }
    if let Some(rest) = trimmed.strip_prefix("NO") {
        return Some((ResponseStatus::No, rest.trim().to_string()));
    }
    trimmed
        .strip_prefix("BYE")
        .map(|rest| (ResponseStatus::Bye, rest.trim().to_string()))
}

/// Parse a single capability line, e.g. `"SASL" "PLAIN LOGIN"` or
/// `"STARTTLS"`. Returns `(name, value)` where `value` is `None` for a
/// bare capability and `Some(payload)` for the two-atom form.
pub fn parse_capability_line(line: &str) -> Option<(String, Option<String>)> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let mut parts = SieveTokenizer::new(trimmed);
    let first = parts.next_quoted()?;
    let second = parts.next_quoted();
    Some((first, second))
}

/// Tokenizer for the small subset of ManageSieve atoms Envelope needs to
/// read: quoted strings only. ManageSieve also has literals and atoms, but
/// the capability banner exclusively uses double-quoted strings in
/// practice and in the RFC's ABNF examples.
struct SieveTokenizer<'a> {
    rest: &'a str,
}

impl<'a> SieveTokenizer<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            rest: input.trim_start(),
        }
    }

    fn next_quoted(&mut self) -> Option<String> {
        let rest = self.rest.trim_start();
        let bytes = rest.as_bytes();
        if bytes.first()? != &b'"' {
            return None;
        }
        let mut out = String::new();
        let mut i = 1;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if b == b'"' {
                self.rest = &rest[i + 1..];
                self.rest = self.rest.trim_start();
                return Some(out);
            }
            out.push(b as char);
            i += 1;
        }
        None
    }
}

/// Apply one capability line into a [`Capabilities`] accumulator.
pub fn apply_capability(caps: &mut Capabilities, name: &str, value: Option<&str>) {
    match name.to_ascii_uppercase().as_str() {
        "IMPLEMENTATION" => {
            caps.implementation = value.map(|v| v.to_string());
        }
        "SIEVE" => {
            if let Some(v) = value {
                caps.sieve_extensions = v.split_whitespace().map(|s| s.to_string()).collect();
            }
        }
        "SASL" => {
            if let Some(v) = value {
                caps.sasl_mechanisms = v.split_whitespace().map(|s| s.to_string()).collect();
            }
        }
        "STARTTLS" => {
            caps.starttls = true;
        }
        "VERSION" => {
            caps.version = value.map(|v| v.to_string());
        }
        _ => {}
    }
}

/// Pure dry-run plan: describes what `publish_script` would do against the
/// resolved endpoint, without opening a socket.
///
/// The shape is stable JSON-able data exposed in CLI/MCP `--json` output.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublishPlan {
    pub status: &'static str,
    pub mode: &'static str,
    pub account_id: String,
    pub host: String,
    pub port: u16,
    pub script_name: String,
    pub script: String,
    pub skipped: Vec<String>,
    pub exported_count: usize,
    pub would_upload: bool,
    pub confirm_required: bool,
    pub network_used: bool,
}

/// Build a dry-run plan from already-resolved inputs. Network-free.
#[allow(clippy::too_many_arguments)]
pub fn build_plan(
    account_id: &str,
    host: &str,
    port: u16,
    script_name: &str,
    script: String,
    skipped: Vec<String>,
    exported_count: usize,
) -> PublishPlan {
    PublishPlan {
        status: "dry_run",
        mode: "dry-run",
        account_id: account_id.to_string(),
        host: host.to_string(),
        port,
        script_name: script_name.to_string(),
        script,
        skipped,
        exported_count,
        would_upload: true,
        confirm_required: true,
        network_used: false,
    }
}

/// Result of a successful confirmed `publish_script`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublishResult {
    pub status: &'static str,
    pub mode: &'static str,
    pub account_id: String,
    pub host: String,
    pub port: u16,
    pub script_name: String,
    pub exported_count: usize,
    pub skipped: Vec<String>,
    pub server_implementation: Option<String>,
    pub active_script: String,
    pub starttls_used: bool,
    pub sasl_mechanism: &'static str,
}

/// Hard upper bound on a single script the CLI will publish. Migadu's
/// Pigeonhole defaults allow scripts well over a megabyte; this is a
/// sanity guard against accidentally uploading something that is clearly
/// not a Sieve script.
pub const MAX_SCRIPT_BYTES: usize = 256 * 1024;

/// Publish a Sieve script to ManageSieve at `host:port` for the given
/// account. Performs:
///
/// 1. TCP connect
/// 2. Read plaintext capability banner
/// 3. `STARTTLS` (mandatory — refuses to send credentials otherwise)
/// 4. TLS handshake using the system root store
/// 5. Re-read capability banner
/// 6. `AUTHENTICATE "PLAIN" "<base64>"`
/// 7. `PUTSCRIPT "<name>" {N+}\r\n<bytes>`
/// 8. `SETACTIVE "<name>"`
/// 9. `LOGOUT`
///
/// The protocol transcript is never logged. Returns a [`PublishResult`]
/// with stable fields safe for JSON output.
#[allow(clippy::too_many_arguments)]
pub async fn publish_script(
    account: &AccountWithCredentials,
    host: &str,
    port: u16,
    script_name: &str,
    script: &str,
    exported_count: usize,
    skipped: Vec<String>,
    timeout: Duration,
) -> Result<PublishResult, ManageSieveError> {
    if script.len() > MAX_SCRIPT_BYTES {
        return Err(ManageSieveError::Protocol(format!(
            "script is {} bytes; refusing to upload more than {} bytes",
            script.len(),
            MAX_SCRIPT_BYTES
        )));
    }
    let _quoted_name = sieve_quoted(script_name).ok_or_else(|| {
        ManageSieveError::Protocol("script name must not contain CR or LF".to_string())
    })?;

    let tcp = time::timeout(timeout, TcpStream::connect((host, port)))
        .await
        .map_err(|_| ManageSieveError::Connection(format!("timeout connecting to {host}:{port}")))?
        .map_err(|e| ManageSieveError::Connection(format!("{host}:{port}: {e}")))?;

    let mut plain = BufStream::new(tcp);
    let plain_caps = read_capabilities(&mut plain, timeout).await?;

    if !plain_caps.starttls {
        return Err(ManageSieveError::CapabilityUnavailable(
            "server did not advertise STARTTLS; refusing to send credentials".to_string(),
        ));
    }

    write_line(&mut plain, b"STARTTLS\r\n", timeout).await?;
    expect_ok(&mut plain, "STARTTLS", timeout).await?;

    let tls_stream = upgrade_to_tls(plain.into_inner(), host).await?;
    let mut tls = BufStream::new(tls_stream);

    let tls_caps = read_capabilities(&mut tls, timeout).await?;
    if !tls_caps.supports_sasl("PLAIN") {
        return Err(ManageSieveError::CapabilityUnavailable(
            "server did not advertise SASL PLAIN after STARTTLS".to_string(),
        ));
    }

    let initial = sasl_plain_initial_response(
        account.effective_imap_username(),
        account.effective_imap_password(),
    );
    let auth_cmd = format!("AUTHENTICATE \"PLAIN\" \"{initial}\"\r\n");
    write_line(&mut tls, auth_cmd.as_bytes(), timeout).await?;
    match read_final(&mut tls, timeout).await? {
        (ResponseStatus::Ok, _) => {}
        (ResponseStatus::No, _) => return Err(ManageSieveError::Auth),
        (ResponseStatus::Bye, reason) => {
            return Err(ManageSieveError::Refused(format!(
                "BYE after AUTH: {reason}"
            )));
        }
    }

    let putscript_header = format!("PUTSCRIPT \"{}\" ", escape_quoted_inner(script_name));
    let mut framed = Vec::with_capacity(putscript_header.len() + script.len() + 16);
    framed.extend_from_slice(putscript_header.as_bytes());
    framed.extend_from_slice(&sieve_literal(script.as_bytes()));
    framed.extend_from_slice(b"\r\n");
    write_line(&mut tls, &framed, timeout).await?;
    expect_ok(&mut tls, "PUTSCRIPT", timeout).await?;

    let setactive = format!("SETACTIVE \"{}\"\r\n", escape_quoted_inner(script_name));
    write_line(&mut tls, setactive.as_bytes(), timeout).await?;
    expect_ok(&mut tls, "SETACTIVE", timeout).await?;

    write_line(&mut tls, b"LOGOUT\r\n", timeout).await?;
    // LOGOUT is best-effort; ignore parse errors after we sent the bytes.
    let _ = time::timeout(timeout, read_final(&mut tls, timeout)).await;

    Ok(PublishResult {
        status: "published",
        mode: "confirmed",
        account_id: account.account.id.clone(),
        host: host.to_string(),
        port,
        script_name: script_name.to_string(),
        exported_count,
        skipped,
        server_implementation: tls_caps.implementation,
        active_script: script_name.to_string(),
        starttls_used: true,
        sasl_mechanism: "PLAIN",
    })
}

fn escape_quoted_inner(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn write_line<W>(
    stream: &mut BufStream<W>,
    bytes: &[u8],
    timeout: Duration,
) -> Result<(), ManageSieveError>
where
    W: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    time::timeout(timeout, async {
        stream.write_all(bytes).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| ManageSieveError::Connection("timeout writing to ManageSieve".to_string()))?
    .map_err(|e| ManageSieveError::Connection(format!("write: {e}")))
}

async fn read_line<R>(
    stream: &mut BufStream<R>,
    timeout: Duration,
) -> Result<String, ManageSieveError>
where
    R: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut buf = Vec::with_capacity(256);
    time::timeout(timeout, async {
        loop {
            let mut byte = [0u8; 1];
            let n = stream.read(&mut byte).await?;
            if n == 0 {
                if buf.is_empty() {
                    return Err::<Vec<u8>, std::io::Error>(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "EOF before line terminator",
                    ));
                }
                return Ok(buf);
            }
            buf.push(byte[0]);
            if byte[0] == b'\n' {
                return Ok(buf);
            }
            if buf.len() > 64 * 1024 {
                return Err::<Vec<u8>, std::io::Error>(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line too long",
                ));
            }
        }
    })
    .await
    .map_err(|_| ManageSieveError::Connection("timeout reading from ManageSieve".to_string()))?
    .map_err(|e| ManageSieveError::Connection(format!("read: {e}")))
    .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
}

async fn read_capabilities<S>(
    stream: &mut BufStream<S>,
    timeout: Duration,
) -> Result<Capabilities, ManageSieveError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut caps = Capabilities::default();
    loop {
        let line = read_line(stream, timeout).await?;
        if let Some((status, reason)) = classify_response(&line) {
            return match status {
                ResponseStatus::Ok => Ok(caps),
                ResponseStatus::No => Err(ManageSieveError::Refused(reason)),
                ResponseStatus::Bye => Err(ManageSieveError::Refused(format!("BYE: {reason}"))),
            };
        }
        if let Some((name, value)) = parse_capability_line(&line) {
            apply_capability(&mut caps, &name, value.as_deref());
        }
    }
}

async fn read_final<S>(
    stream: &mut BufStream<S>,
    timeout: Duration,
) -> Result<(ResponseStatus, String), ManageSieveError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let line = read_line(stream, timeout).await?;
        if let Some(parsed) = classify_response(&line) {
            return Ok(parsed);
        }
        // Skip untagged data lines (e.g. literal payloads, capability
        // refreshes after AUTHENTICATE).
        if line.trim().is_empty() {
            return Err(ManageSieveError::Protocol("empty line".to_string()));
        }
    }
}

async fn expect_ok<S>(
    stream: &mut BufStream<S>,
    command: &str,
    timeout: Duration,
) -> Result<(), ManageSieveError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (status, reason) = read_final(stream, timeout).await?;
    match status {
        ResponseStatus::Ok => Ok(()),
        ResponseStatus::No => Err(ManageSieveError::Refused(format!("{command}: {reason}"))),
        ResponseStatus::Bye => Err(ManageSieveError::Refused(format!(
            "{command} BYE: {reason}"
        ))),
    }
}

async fn upgrade_to_tls(
    tcp: TcpStream,
    host: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, ManageSieveError> {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from(host)
        .map_err(|e| ManageSieveError::Connection(format!("invalid server name {host}: {e}")))?
        .to_owned();
    connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| ManageSieveError::Connection(format!("TLS handshake with {host}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migadu_defaults_match_canonical_host() {
        let got = migadu_defaults("imap.migadu.com").expect("migadu host should resolve");
        assert_eq!(got.0, "sieve.migadu.com");
        assert_eq!(got.1, 4190);
    }

    #[test]
    fn migadu_defaults_case_insensitive() {
        let got = migadu_defaults("Imap.Migadu.Com").expect("migadu host should resolve");
        assert_eq!(got.0, "sieve.migadu.com");
        assert_eq!(got.1, 4190);
    }

    #[test]
    fn migadu_defaults_skip_non_migadu_hosts() {
        assert!(migadu_defaults("imap.gmail.com").is_none());
        assert!(migadu_defaults("imap.example.com").is_none());
        assert!(migadu_defaults("").is_none());
    }

    #[test]
    fn resolve_endpoint_uses_migadu_defaults_when_no_override() {
        let (host, port) = resolve_sieve_endpoint("imap.migadu.com", None, None);
        assert_eq!(host, "sieve.migadu.com");
        assert_eq!(port, 4190);
    }

    #[test]
    fn resolve_endpoint_falls_back_to_imap_host_for_unknown_provider() {
        let (host, port) = resolve_sieve_endpoint("mail.example.com", None, None);
        assert_eq!(host, "mail.example.com");
        assert_eq!(port, 4190);
    }

    #[test]
    fn resolve_endpoint_host_override_keeps_default_port() {
        let (host, port) =
            resolve_sieve_endpoint("imap.migadu.com", Some("sieve.example.com"), None);
        assert_eq!(host, "sieve.example.com");
        assert_eq!(port, 4190);
    }

    #[test]
    fn resolve_endpoint_port_override_keeps_default_host() {
        let (host, port) = resolve_sieve_endpoint("imap.migadu.com", None, Some(4191));
        assert_eq!(host, "sieve.migadu.com");
        assert_eq!(port, 4191);
    }

    #[test]
    fn resolve_endpoint_both_overrides_win() {
        let (host, port) =
            resolve_sieve_endpoint("imap.migadu.com", Some("sieve.alt.example"), Some(2000));
        assert_eq!(host, "sieve.alt.example");
        assert_eq!(port, 2000);
    }

    #[test]
    fn sieve_quoted_escapes_backslash_and_quote() {
        let got = sieve_quoted(r#"Say "no" \stop"#).expect("plain ASCII should quote");
        assert_eq!(got, r#""Say \"no\" \\stop""#);
    }

    #[test]
    fn sieve_quoted_refuses_line_breaks() {
        assert!(sieve_quoted("line1\nline2").is_none());
        assert!(sieve_quoted("line1\rline2").is_none());
    }

    #[test]
    fn sieve_literal_uses_non_synchronizing_form_with_byte_length() {
        let payload = b"require [\"fileinto\"];\n";
        let framed = sieve_literal(payload);
        let framed_str = String::from_utf8(framed).unwrap();
        let header = format!("{{{}+}}\r\n", payload.len());
        assert!(framed_str.starts_with(&header), "got: {framed_str}");
        assert!(
            framed_str.ends_with(std::str::from_utf8(payload).unwrap()),
            "got: {framed_str}"
        );
    }

    #[test]
    fn sieve_literal_byte_length_is_utf8_bytes_not_chars() {
        // Embedded non-ASCII reason text should be measured in bytes, not chars.
        let payload = "naïve".as_bytes();
        let framed = sieve_literal(payload);
        let framed_str = String::from_utf8(framed).unwrap();
        // "naïve" is 6 UTF-8 bytes (n=1, a=1, ï=2, v=1, e=1).
        assert!(framed_str.starts_with("{6+}\r\n"), "got: {framed_str}");
    }

    #[test]
    fn sasl_plain_encodes_authcid_and_password() {
        // Per RFC 4616: \0<authcid>\0<password>, base64-encoded.
        let encoded = sasl_plain_initial_response("alice@example.com", "hunter2");
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(decoded[0], 0);
        let rest = &decoded[1..];
        let sep = rest.iter().position(|b| *b == 0).expect("second NUL");
        assert_eq!(&rest[..sep], b"alice@example.com");
        assert_eq!(&rest[sep + 1..], b"hunter2");
    }

    #[test]
    fn classify_response_recognizes_ok_no_bye() {
        assert_eq!(
            classify_response("OK \"capability\""),
            Some((ResponseStatus::Ok, "\"capability\"".to_string()))
        );
        assert_eq!(
            classify_response("NO \"bad mech\""),
            Some((ResponseStatus::No, "\"bad mech\"".to_string()))
        );
        assert_eq!(
            classify_response("BYE \"timeout\""),
            Some((ResponseStatus::Bye, "\"timeout\"".to_string()))
        );
        assert_eq!(
            classify_response("OK"),
            Some((ResponseStatus::Ok, "".to_string()))
        );
        assert_eq!(classify_response("\"IMPLEMENTATION\" \"Dovecot\""), None);
    }

    #[test]
    fn parse_capability_line_two_atom_form() {
        let got = parse_capability_line("\"IMPLEMENTATION\" \"Dovecot Pigeonhole\"\r\n").unwrap();
        assert_eq!(got.0, "IMPLEMENTATION");
        assert_eq!(got.1.as_deref(), Some("Dovecot Pigeonhole"));
    }

    #[test]
    fn parse_capability_line_one_atom_form() {
        let got = parse_capability_line("\"STARTTLS\"\r\n").unwrap();
        assert_eq!(got.0, "STARTTLS");
        assert_eq!(got.1, None);
    }

    #[test]
    fn apply_capability_collects_sieve_extensions_and_sasl() {
        let mut caps = Capabilities::default();
        apply_capability(&mut caps, "IMPLEMENTATION", Some("Dovecot Pigeonhole"));
        apply_capability(&mut caps, "SIEVE", Some("fileinto reject ereject"));
        apply_capability(&mut caps, "SASL", Some("PLAIN LOGIN"));
        apply_capability(&mut caps, "STARTTLS", None);
        apply_capability(&mut caps, "VERSION", Some("1.0"));
        assert_eq!(caps.implementation.as_deref(), Some("Dovecot Pigeonhole"));
        assert!(caps.sieve_extensions.iter().any(|e| e == "reject"));
        assert!(caps.supports_sasl("plain"));
        assert!(caps.supports_sasl("PLAIN"));
        assert!(caps.starttls);
        assert_eq!(caps.version.as_deref(), Some("1.0"));
    }

    #[test]
    fn build_plan_marks_dry_run_and_confirm_required() {
        let plan = build_plan(
            "acct-1",
            "sieve.migadu.com",
            4190,
            "envelope-rules",
            "require [\"fileinto\"];\n".to_string(),
            vec!["TagOnly".to_string()],
            2,
        );
        assert_eq!(plan.status, "dry_run");
        assert_eq!(plan.mode, "dry-run");
        assert_eq!(plan.account_id, "acct-1");
        assert_eq!(plan.host, "sieve.migadu.com");
        assert_eq!(plan.port, 4190);
        assert_eq!(plan.script_name, "envelope-rules");
        assert!(plan.would_upload);
        assert!(plan.confirm_required);
        assert!(!plan.network_used);
        assert_eq!(plan.exported_count, 2);
        assert_eq!(plan.skipped, vec!["TagOnly".to_string()]);
    }

    #[test]
    fn build_plan_serializes_to_stable_json_keys() {
        let plan = build_plan(
            "acct-1",
            "sieve.migadu.com",
            4190,
            "envelope-rules",
            "stop;\n".to_string(),
            vec![],
            0,
        );
        let value = serde_json::to_value(&plan).unwrap();
        for key in [
            "status",
            "mode",
            "account_id",
            "host",
            "port",
            "script_name",
            "script",
            "skipped",
            "exported_count",
            "would_upload",
            "confirm_required",
            "network_used",
        ] {
            assert!(
                value.get(key).is_some(),
                "expected '{key}' in dry-run JSON: {value}"
            );
        }
        assert_eq!(value["status"], "dry_run");
        assert_eq!(value["mode"], "dry-run");
    }

    #[test]
    fn escape_quoted_inner_handles_special_chars() {
        assert_eq!(escape_quoted_inner("plain"), "plain");
        assert_eq!(escape_quoted_inner(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_quoted_inner(r"a\b"), r"a\\b");
        assert_eq!(escape_quoted_inner(r#"a"b\c"#), r#"a\"b\\c"#);
    }
}
