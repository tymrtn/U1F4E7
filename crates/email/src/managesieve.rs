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
//! - Before changing anything, a publish lists the server's scripts. When a
//!   script other than Envelope's is active, it refuses unless the operator
//!   chose `--keep-existing` (a wrapper that includes that script first,
//!   RFC 6609) or `--replace-active <name>`. See [`decide_activation`].
//! - We only `PUTSCRIPT` the script the operator named and its wrapper, and
//!   `SETACTIVE` one of them. We never `DELETESCRIPT` or `RENAMESCRIPT`.
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

    /// Publishing would switch off a script the operator did not name.
    /// Nothing was uploaded.
    #[error("{}", .0.message)]
    ActiveScriptConflict(ActiveScriptConflict),
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

    pub fn supports_extension(&self, extension: &str) -> bool {
        self.sieve_extensions
            .iter()
            .any(|e| e.eq_ignore_ascii_case(extension))
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
        let mut out = Vec::new();
        let mut i = 1;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            if b == b'"' {
                self.rest = &rest[i + 1..];
                self.rest = self.rest.trim_start();
                return String::from_utf8(out).ok();
            }
            out.push(b);
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

/// One script on the server, as `LISTSCRIPTS` reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServerScript {
    pub name: String,
    pub active: bool,
}

/// What a publish does when a script other than Envelope's is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingScript {
    /// Refuse and name the active script. The default.
    Refuse,
    /// `--keep-existing`: keep it running through a wrapper that includes it
    /// first and then Envelope's script. Needs the server's `include`
    /// extension (RFC 6609).
    Keep,
    /// `--replace-active <name>`: switch off exactly this script. It stays
    /// on the server.
    Replace(String),
}

/// How a publish leaves Envelope's script running on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// No script was active, or Envelope's own script was.
    Activate,
    /// Envelope's wrapper was already active and includes the script.
    KeepWrapper,
    /// The new wrapper runs `previous` first and then Envelope's script.
    WrapExisting { previous: String },
    /// `previous` was switched off. It stays on the server.
    ReplaceActive { previous: String },
}

impl Activation {
    /// Stable name, used in JSON output and the local publish record.
    pub fn kind(&self) -> &'static str {
        match self {
            Activation::Activate => "activated",
            Activation::KeepWrapper => "kept_wrapper",
            Activation::WrapExisting { .. } => "wrapped_existing",
            Activation::ReplaceActive { .. } => "replaced_active",
        }
    }

    /// The user's script this publish wrapped or switched off.
    pub fn previous(&self) -> Option<&str> {
        match self {
            Activation::WrapExisting { previous } | Activation::ReplaceActive { previous } => {
                Some(previous)
            }
            Activation::Activate | Activation::KeepWrapper => None,
        }
    }

    /// The script left active: Envelope's script, or its wrapper.
    pub fn active_script(&self, script_name: &str) -> String {
        match self {
            Activation::KeepWrapper | Activation::WrapExisting { .. } => {
                wrapper_script_name(script_name)
            }
            Activation::Activate | Activation::ReplaceActive { .. } => script_name.to_string(),
        }
    }
}

/// Name of the wrapper `--keep-existing` uploads for `script_name`.
pub fn wrapper_script_name(script_name: &str) -> String {
    format!("{script_name}-wrapper")
}

/// The wrapper: the script that was active first, then Envelope's.
/// `:optional` keeps Envelope's rules running if that script is deleted
/// later.
pub fn wrapper_script(previous: &str, script_name: &str) -> String {
    format!(
        "# Written by Envelope: envelope rule publish-sieve --keep-existing\n\
         # Runs the script that was active before, then Envelope's rules.\n\
         require [\"include\"];\n\
         include :personal :optional \"{previous}\";\n\
         include :personal \"{script_name}\";\n",
        previous = escape_quoted_inner(previous),
        script_name = escape_quoted_inner(script_name),
    )
}

/// A publish refused because it would switch off a script the operator did
/// not name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActiveScriptConflict {
    /// The script active on the server.
    pub active_script: String,
    /// Whether the server supports `include`, which `--keep-existing` needs.
    pub include_supported: bool,
    pub message: String,
}

/// Decide how publishing `script_name` activates it, given the server's
/// scripts. Network-free. It never switches off a script the operator did
/// not name with `--replace-active`.
pub fn decide_activation(
    scripts: &[ServerScript],
    include_supported: bool,
    script_name: &str,
    existing: &ExistingScript,
) -> Result<Activation, ActiveScriptConflict> {
    let Some(active) = scripts.iter().find(|s| s.active).map(|s| s.name.as_str()) else {
        return Ok(Activation::Activate);
    };
    if active == script_name {
        return Ok(Activation::Activate);
    }
    if active == wrapper_script_name(script_name) {
        return Ok(Activation::KeepWrapper);
    }
    let options = if include_supported {
        format!(
            "Use --keep-existing to run \"{active}\" first and then Envelope's rules, \
             or --replace-active \"{active}\" to switch it off (it stays on the server)."
        )
    } else {
        format!(
            "This server does not support Sieve include, so both cannot run. \
             Use --replace-active \"{active}\" to switch it off (it stays on the server)."
        )
    };
    let message = match existing {
        ExistingScript::Keep if include_supported => {
            return Ok(Activation::WrapExisting {
                previous: active.to_string(),
            });
        }
        ExistingScript::Replace(named) if named == active => {
            return Ok(Activation::ReplaceActive {
                previous: active.to_string(),
            });
        }
        ExistingScript::Refuse => format!(
            "\"{active}\" is the active Sieve script on the server and may hold filters set up \
             in your mail provider's settings. Publishing would switch it off. {options}"
        ),
        ExistingScript::Keep => format!(
            "--keep-existing needs Sieve include, which this server does not support, so \
             \"{active}\" cannot keep running alongside Envelope's rules. Use --replace-active \
             \"{active}\" to switch it off (it stays on the server)."
        ),
        ExistingScript::Replace(named) => format!(
            "--replace-active names \"{named}\", but the active Sieve script on the server is \
             \"{active}\". Publishing would switch \"{active}\" off. {options}"
        ),
    };
    Err(ActiveScriptConflict {
        active_script: active.to_string(),
        include_supported,
        message,
    })
}

/// What a confirmed publish does in each state the server can be in. A dry
/// run never connects, so it shows every branch.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActivationPlan {
    /// `refuse`, `keep_existing` or `replace_active`.
    pub on_another_active_script: &'static str,
    pub if_no_script_or_envelope_script_active: String,
    pub if_envelope_wrapper_active: String,
    pub if_another_script_active: String,
    /// Always false: Envelope never deletes a script on the server.
    pub deletes_scripts: bool,
}

pub fn activation_plan(script_name: &str, existing: &ExistingScript) -> ActivationPlan {
    let wrapper = wrapper_script_name(script_name);
    let (choice, other) = match existing {
        ExistingScript::Refuse => (
            "refuse",
            "refuse and name that script; nothing is uploaded. Pass --keep-existing to keep it \
             running, or --replace-active <name> to switch it off"
                .to_string(),
        ),
        ExistingScript::Keep => (
            "keep_existing",
            format!(
                "if the server supports Sieve include: upload \"{script_name}\" without \
                 activating it, upload \"{wrapper}\", which runs that script first and then \
                 \"{script_name}\", and make \"{wrapper}\" active. Without include support: \
                 refuse; nothing is uploaded"
            ),
        ),
        ExistingScript::Replace(named) => (
            "replace_active",
            format!(
                "if it is \"{named}\": switch \"{named}\" off (it stays on the server) and make \
                 \"{script_name}\" active. Any other script: refuse; nothing is uploaded"
            ),
        ),
    };
    ActivationPlan {
        on_another_active_script: choice,
        if_no_script_or_envelope_script_active: format!(
            "upload \"{script_name}\" and make it the active script"
        ),
        if_envelope_wrapper_active: format!(
            "upload \"{script_name}\"; \"{wrapper}\" stays active and still runs the earlier \
             script first"
        ),
        if_another_script_active: other,
        deletes_scripts: false,
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
    pub activation: ActivationPlan,
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
    existing: &ExistingScript,
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
        activation: activation_plan(script_name, existing),
    }
}

/// What a confirmed publish did on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOutcome {
    pub activation: Activation,
    /// The script now active: Envelope's script or its wrapper.
    pub active_script: String,
    pub include_supported: bool,
    pub server_implementation: Option<String>,
}

/// A publish attempt: the server's scripts as listed before anything
/// changed, and the result. `scripts_before` is `None` when the session
/// failed before `LISTSCRIPTS` answered.
#[derive(Debug)]
pub struct PublishAttempt {
    pub scripts_before: Option<Vec<ServerScript>>,
    pub result: Result<PublishOutcome, ManageSieveError>,
}

impl PublishAttempt {
    fn failed(error: ManageSieveError) -> Self {
        PublishAttempt {
            scripts_before: None,
            result: Err(error),
        }
    }
}

/// What `LISTSCRIPTS` and the capability banner report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub scripts: Vec<ServerScript>,
    pub include_supported: bool,
    pub server_implementation: Option<String>,
}

/// Hard upper bound on a single script the CLI will publish. Migadu's
/// Pigeonhole defaults allow scripts well over a megabyte; this is a
/// sanity guard against accidentally uploading something that is clearly
/// not a Sieve script.
pub const MAX_SCRIPT_BYTES: usize = 256 * 1024;

/// Longest script name read from a `LISTSCRIPTS` literal.
const MAX_SCRIPT_NAME_BYTES: usize = 1024;

/// Publish a Sieve script to ManageSieve at `host:port` for the given
/// account. Performs:
///
/// 1. TCP connect and read the plaintext capability banner
/// 2. `STARTTLS` (mandatory — refuses to send credentials otherwise), the
///    TLS handshake using the system root store, and the re-issued banner
/// 3. `AUTHENTICATE "PLAIN" "<base64>"`
/// 4. [`publish_on_session`]: `LISTSCRIPTS`, then `PUTSCRIPT` and
///    `SETACTIVE` as [`decide_activation`] allows
/// 5. `LOGOUT`
///
/// The protocol transcript is never logged.
pub async fn publish_script(
    account: &AccountWithCredentials,
    host: &str,
    port: u16,
    script_name: &str,
    script: &str,
    existing: &ExistingScript,
    timeout: Duration,
) -> PublishAttempt {
    if script.len() > MAX_SCRIPT_BYTES {
        return PublishAttempt::failed(ManageSieveError::Protocol(format!(
            "script is {} bytes; refusing to upload more than {} bytes",
            script.len(),
            MAX_SCRIPT_BYTES
        )));
    }
    if sieve_quoted(script_name).is_none() {
        return PublishAttempt::failed(ManageSieveError::Protocol(
            "script name must not contain CR or LF".to_string(),
        ));
    }
    let (mut session, caps) = match connect_authenticated(account, host, port, timeout).await {
        Ok(connected) => connected,
        Err(e) => return PublishAttempt::failed(e),
    };
    let attempt =
        publish_on_session(&mut session, &caps, script_name, script, existing, timeout).await;
    logout(&mut session, timeout).await;
    attempt
}

/// Publish on an authenticated session. Lists the server's scripts first
/// and uploads nothing when [`decide_activation`] refuses. Writes only
/// `script_name` and its wrapper; never deletes or renames a script.
pub async fn publish_on_session<S>(
    stream: &mut BufStream<S>,
    caps: &Capabilities,
    script_name: &str,
    script: &str,
    existing: &ExistingScript,
    timeout: Duration,
) -> PublishAttempt
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let scripts = match list_scripts(stream, timeout).await {
        Ok(scripts) => scripts,
        Err(e) => return PublishAttempt::failed(e),
    };
    let include_supported = caps.supports_extension("include");
    let result = apply_activation(
        stream,
        &scripts,
        include_supported,
        script_name,
        script,
        existing,
        timeout,
    )
    .await
    .map(|activation| PublishOutcome {
        active_script: activation.active_script(script_name),
        activation,
        include_supported,
        server_implementation: caps.implementation.clone(),
    });
    PublishAttempt {
        scripts_before: Some(scripts),
        result,
    }
}

async fn apply_activation<S>(
    stream: &mut BufStream<S>,
    scripts: &[ServerScript],
    include_supported: bool,
    script_name: &str,
    script: &str,
    existing: &ExistingScript,
    timeout: Duration,
) -> Result<Activation, ManageSieveError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let activation = decide_activation(scripts, include_supported, script_name, existing)
        .map_err(ManageSieveError::ActiveScriptConflict)?;
    put_script(stream, script_name, script, timeout).await?;
    match &activation {
        Activation::Activate | Activation::ReplaceActive { .. } => {
            set_active(stream, script_name, timeout).await?;
        }
        Activation::KeepWrapper => {}
        Activation::WrapExisting { previous } => {
            let wrapper = wrapper_script_name(script_name);
            put_script(
                stream,
                &wrapper,
                &wrapper_script(previous, script_name),
                timeout,
            )
            .await?;
            set_active(stream, &wrapper, timeout).await?;
        }
    }
    Ok(activation)
}

/// Read the server's scripts. After authenticating, this sends only
/// `LISTSCRIPTS` and `LOGOUT`; nothing on the server changes.
pub async fn read_status(
    account: &AccountWithCredentials,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<ServerStatus, ManageSieveError> {
    let (mut session, caps) = connect_authenticated(account, host, port, timeout).await?;
    let scripts = list_scripts(&mut session, timeout).await;
    logout(&mut session, timeout).await;
    Ok(ServerStatus {
        scripts: scripts?,
        include_supported: caps.supports_extension("include"),
        server_implementation: caps.implementation,
    })
}

/// `LISTSCRIPTS` on an authenticated session. Each name arrives as a quoted
/// string or a literal (RFC 5804 §2.7); the active one carries `ACTIVE`.
pub async fn list_scripts<S>(
    stream: &mut BufStream<S>,
    timeout: Duration,
) -> Result<Vec<ServerScript>, ManageSieveError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    write_line(stream, b"LISTSCRIPTS\r\n", timeout).await?;
    let mut scripts = Vec::new();
    loop {
        let line = read_line(stream, timeout).await?;
        if let Some((status, reason)) = classify_response(&line) {
            return match status {
                ResponseStatus::Ok => Ok(scripts),
                ResponseStatus::No => {
                    Err(ManageSieveError::Refused(format!("LISTSCRIPTS: {reason}")))
                }
                ResponseStatus::Bye => Err(ManageSieveError::Refused(format!(
                    "LISTSCRIPTS BYE: {reason}"
                ))),
            };
        }
        let (name, rest) = match literal_length(&line) {
            Some(len) if len > MAX_SCRIPT_NAME_BYTES => {
                return Err(ManageSieveError::Protocol(format!(
                    "LISTSCRIPTS sent a {len}-byte script name"
                )));
            }
            Some(len) => {
                let bytes = read_exact_bytes(stream, len, timeout).await?;
                let name = String::from_utf8(bytes).map_err(|_| {
                    ManageSieveError::Protocol("LISTSCRIPTS sent a name that is not UTF-8".into())
                })?;
                (name, read_line(stream, timeout).await?)
            }
            None => {
                let mut tokens = SieveTokenizer::new(line.trim_end_matches(['\r', '\n']));
                let name = tokens.next_quoted().ok_or_else(|| {
                    ManageSieveError::Protocol(format!(
                        "unexpected LISTSCRIPTS line: {}",
                        line.trim_end()
                    ))
                })?;
                (name, tokens.rest.to_string())
            }
        };
        // `read_line` decodes lossily; a replaced byte means the name we
        // hold is not the server's, and the wrapper must include the real one.
        if name.contains(char::REPLACEMENT_CHARACTER) {
            return Err(ManageSieveError::Protocol(
                "LISTSCRIPTS sent a name that is not UTF-8".into(),
            ));
        }
        let active = match rest.trim() {
            "" => false,
            word if word.eq_ignore_ascii_case("ACTIVE") => true,
            other => {
                return Err(ManageSieveError::Protocol(format!(
                    "unexpected LISTSCRIPTS suffix: {other}"
                )));
            }
        };
        scripts.push(ServerScript { name, active });
    }
}

/// `{N}` or `{N+}` on its own line: a literal of N bytes follows.
fn literal_length(line: &str) -> Option<usize> {
    let inner = line
        .trim_end_matches(['\r', '\n'])
        .strip_prefix('{')?
        .strip_suffix('}')?;
    inner.strip_suffix('+').unwrap_or(inner).parse().ok()
}

/// Connect, `STARTTLS`, and authenticate with SASL PLAIN. Returns the
/// session and the capabilities advertised after `STARTTLS`.
async fn connect_authenticated(
    account: &AccountWithCredentials,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<
    (
        BufStream<tokio_rustls::client::TlsStream<TcpStream>>,
        Capabilities,
    ),
    ManageSieveError,
> {
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
        (ResponseStatus::Ok, _) => Ok((tls, tls_caps)),
        (ResponseStatus::No, _) => Err(ManageSieveError::Auth),
        (ResponseStatus::Bye, reason) => Err(ManageSieveError::Refused(format!(
            "BYE after AUTH: {reason}"
        ))),
    }
}

async fn put_script<S>(
    stream: &mut BufStream<S>,
    name: &str,
    script: &str,
    timeout: Duration,
) -> Result<(), ManageSieveError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let header = format!("PUTSCRIPT \"{}\" ", escape_quoted_inner(name));
    let mut framed = Vec::with_capacity(header.len() + script.len() + 16);
    framed.extend_from_slice(header.as_bytes());
    framed.extend_from_slice(&sieve_literal(script.as_bytes()));
    framed.extend_from_slice(b"\r\n");
    write_line(stream, &framed, timeout).await?;
    expect_ok(stream, "PUTSCRIPT", timeout).await
}

async fn set_active<S>(
    stream: &mut BufStream<S>,
    name: &str,
    timeout: Duration,
) -> Result<(), ManageSieveError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let command = format!("SETACTIVE \"{}\"\r\n", escape_quoted_inner(name));
    write_line(stream, command.as_bytes(), timeout).await?;
    expect_ok(stream, "SETACTIVE", timeout).await
}

/// Best effort: by now the work is done or has already failed.
async fn logout<S>(stream: &mut BufStream<S>, timeout: Duration)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if write_line(stream, b"LOGOUT\r\n", timeout).await.is_ok() {
        let _ = read_final(stream, timeout).await;
    }
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

async fn read_exact_bytes<S>(
    stream: &mut BufStream<S>,
    len: usize,
    timeout: Duration,
) -> Result<Vec<u8>, ManageSieveError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; len];
    time::timeout(timeout, stream.read_exact(&mut buf))
        .await
        .map_err(|_| ManageSieveError::Connection("timeout reading from ManageSieve".to_string()))?
        .map_err(|e| ManageSieveError::Connection(format!("read: {e}")))?;
    Ok(buf)
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
            &ExistingScript::Refuse,
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
            &ExistingScript::Refuse,
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
            "activation",
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
    // ── Publishing against a fake ManageSieve server ──────────────────

    use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream};

    const T: Duration = Duration::from_secs(5);
    const SCRIPT: &str = "require [\"fileinto\"];\n\nif address :is \"from\" \"a@b.example\" {\n    fileinto \"Archive\";\n    stop;\n}\n";

    /// What the fake server received, and the scripts it holds at the end.
    #[derive(Debug, Default)]
    struct ServerLog {
        /// Each command's verb and its first quoted argument, in order.
        commands: Vec<String>,
        /// Uploaded script bodies, by name.
        uploads: Vec<(String, String)>,
        scripts: Vec<ServerScript>,
    }

    /// A ManageSieve server past STARTTLS and AUTHENTICATE, at the far end
    /// of an in-memory pipe. It answers LISTSCRIPTS, PUTSCRIPT and SETACTIVE
    /// and refuses everything else.
    async fn fake_server(stream: DuplexStream, scripts: Vec<ServerScript>) -> ServerLog {
        let (read, mut write) = tokio::io::split(stream);
        let mut read = BufReader::new(read);
        let mut log = ServerLog {
            scripts,
            ..ServerLog::default()
        };
        loop {
            let mut line = String::new();
            if read.read_line(&mut line).await.unwrap() == 0 {
                return log;
            }
            let line = line.trim_end().to_string();
            let (verb, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            let name = rest.split('"').nth(1).unwrap_or("").to_string();
            log.commands
                .push(format!("{verb} {name}").trim_end().to_string());
            let reply = match verb {
                "LISTSCRIPTS" => {
                    let mut out = String::new();
                    for s in &log.scripts {
                        let active = if s.active { " ACTIVE" } else { "" };
                        out.push_str(&format!("\"{}\"{active}\r\n", s.name));
                    }
                    out + "OK \"Listscripts completed.\"\r\n"
                }
                "PUTSCRIPT" => {
                    let len: usize = rest
                        .rsplit('{')
                        .next()
                        .unwrap()
                        .trim_end_matches("+}")
                        .parse()
                        .unwrap();
                    let mut body = vec![0u8; len];
                    read.read_exact(&mut body).await.unwrap();
                    let mut crlf = String::new();
                    read.read_line(&mut crlf).await.unwrap();
                    if !log.scripts.iter().any(|s| s.name == name) {
                        log.scripts.push(ServerScript {
                            name: name.clone(),
                            active: false,
                        });
                    }
                    log.uploads.push((name, String::from_utf8(body).unwrap()));
                    "OK\r\n".to_string()
                }
                "SETACTIVE" => {
                    for s in &mut log.scripts {
                        s.active = s.name == name;
                    }
                    "OK\r\n".to_string()
                }
                _ => "NO \"not supported here\"\r\n".to_string(),
            };
            write.write_all(reply.as_bytes()).await.unwrap();
        }
    }

    fn server_scripts(list: &[(&str, bool)]) -> Vec<ServerScript> {
        list.iter()
            .map(|(name, active)| ServerScript {
                name: name.to_string(),
                active: *active,
            })
            .collect()
    }

    fn caps(include: bool) -> Capabilities {
        let mut sieve_extensions = vec!["fileinto".to_string(), "reject".to_string()];
        if include {
            sieve_extensions.push("include".to_string());
        }
        Capabilities {
            implementation: Some("Fake Pigeonhole".to_string()),
            sieve_extensions,
            ..Capabilities::default()
        }
    }

    async fn publish_against(
        scripts: Vec<ServerScript>,
        include: bool,
        existing: ExistingScript,
    ) -> (PublishAttempt, ServerLog) {
        let (client, server) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(fake_server(server, scripts));
        let mut stream = BufStream::new(client);
        let attempt = publish_on_session(
            &mut stream,
            &caps(include),
            "envelope-rules",
            SCRIPT,
            &existing,
            T,
        )
        .await;
        drop(stream);
        (attempt, server.await.unwrap())
    }

    /// Envelope only lists, uploads and activates. It never deletes or
    /// renames a script, and the user's script is still on the server.
    fn assert_kept(log: &ServerLog, user_script: &str) {
        for command in &log.commands {
            let verb = command.split(' ').next().unwrap();
            assert!(
                matches!(verb, "LISTSCRIPTS" | "PUTSCRIPT" | "SETACTIVE"),
                "unexpected {command}: {:?}",
                log.commands
            );
        }
        assert!(
            log.scripts.iter().any(|s| s.name == user_script),
            "{user_script} must stay on the server: {:?}",
            log.scripts
        );
    }

    fn conflict(attempt: PublishAttempt) -> ActiveScriptConflict {
        match attempt.result {
            Err(ManageSieveError::ActiveScriptConflict(c)) => c,
            other => panic!("expected an active-script refusal, got {other:?}"),
        }
    }

    fn active(log: &ServerLog) -> Option<&str> {
        log.scripts
            .iter()
            .find(|s| s.active)
            .map(|s| s.name.as_str())
    }

    #[tokio::test]
    async fn another_active_script_is_refused_before_anything_is_uploaded() {
        let scripts = server_scripts(&[("roundcube", true), ("vacation", false)]);
        let (attempt, log) = publish_against(scripts.clone(), true, ExistingScript::Refuse).await;

        assert_eq!(attempt.scripts_before.as_ref(), Some(&scripts));
        let c = conflict(attempt);
        assert_eq!(c.active_script, "roundcube");
        assert!(c.include_supported);
        assert!(
            c.message
                .contains("\"roundcube\" is the active Sieve script")
                && c.message.contains("would switch it off")
                && c.message.contains("--keep-existing")
                && c.message.contains("--replace-active \"roundcube\""),
            "{}",
            c.message
        );
        assert_eq!(log.commands, ["LISTSCRIPTS"]);
        assert_eq!(active(&log), Some("roundcube"));
        assert_kept(&log, "roundcube");
    }

    #[tokio::test]
    async fn keep_existing_wraps_the_active_script_when_the_server_supports_include() {
        let (attempt, log) = publish_against(
            server_scripts(&[("roundcube", true), ("vacation", false)]),
            true,
            ExistingScript::Keep,
        )
        .await;

        let outcome = attempt.result.expect("publish");
        assert_eq!(
            outcome.activation,
            Activation::WrapExisting {
                previous: "roundcube".to_string()
            }
        );
        assert_eq!(outcome.active_script, "envelope-rules-wrapper");
        assert_eq!(
            log.commands,
            [
                "LISTSCRIPTS",
                "PUTSCRIPT envelope-rules",
                "PUTSCRIPT envelope-rules-wrapper",
                "SETACTIVE envelope-rules-wrapper",
            ]
        );
        assert_eq!(
            log.uploads[0],
            ("envelope-rules".to_string(), SCRIPT.to_string())
        );
        let wrapper = &log.uploads[1].1;
        assert_eq!(wrapper, &wrapper_script("roundcube", "envelope-rules"));
        let theirs = wrapper
            .find("include :personal :optional \"roundcube\";")
            .expect("wrapper includes the user's script");
        let ours = wrapper
            .find("include :personal \"envelope-rules\";")
            .expect("wrapper includes Envelope's script");
        assert!(theirs < ours, "the user's script runs first:\n{wrapper}");
        assert!(wrapper.contains("require [\"include\"];"));
        assert_eq!(active(&log), Some("envelope-rules-wrapper"));
        assert_kept(&log, "roundcube");
        assert_kept(&log, "vacation");
    }

    #[tokio::test]
    async fn without_include_keep_existing_is_refused() {
        let (attempt, log) = publish_against(
            server_scripts(&[("roundcube", true)]),
            false,
            ExistingScript::Keep,
        )
        .await;

        let c = conflict(attempt);
        assert_eq!(c.active_script, "roundcube");
        assert!(!c.include_supported);
        assert!(
            c.message.contains("does not support")
                && c.message.contains("--replace-active \"roundcube\""),
            "{}",
            c.message
        );
        assert_eq!(log.commands, ["LISTSCRIPTS"]);
        assert_eq!(active(&log), Some("roundcube"));
    }

    #[tokio::test]
    async fn without_include_the_default_refusal_offers_only_replace_active() {
        let (attempt, _log) = publish_against(
            server_scripts(&[("roundcube", true)]),
            false,
            ExistingScript::Refuse,
        )
        .await;
        let c = conflict(attempt);
        assert!(!c.message.contains("--keep-existing"), "{}", c.message);
        assert!(c.message.contains("--replace-active \"roundcube\""));
    }

    #[tokio::test]
    async fn replace_active_switches_off_only_the_named_script_and_keeps_it() {
        let (attempt, log) = publish_against(
            server_scripts(&[("roundcube", true)]),
            false,
            ExistingScript::Replace("roundcube".to_string()),
        )
        .await;

        let outcome = attempt.result.expect("publish");
        assert_eq!(
            outcome.activation,
            Activation::ReplaceActive {
                previous: "roundcube".to_string()
            }
        );
        assert_eq!(outcome.active_script, "envelope-rules");
        assert_eq!(
            log.commands,
            [
                "LISTSCRIPTS",
                "PUTSCRIPT envelope-rules",
                "SETACTIVE envelope-rules"
            ]
        );
        assert_eq!(active(&log), Some("envelope-rules"));
        assert_kept(&log, "roundcube");
    }

    #[tokio::test]
    async fn replace_active_naming_a_different_script_is_refused() {
        let (attempt, log) = publish_against(
            server_scripts(&[("roundcube", true)]),
            true,
            ExistingScript::Replace("old-filters".to_string()),
        )
        .await;

        let c = conflict(attempt);
        assert!(
            c.message.contains("\"old-filters\"") && c.message.contains("\"roundcube\""),
            "{}",
            c.message
        );
        assert_eq!(log.commands, ["LISTSCRIPTS"]);
        assert_eq!(active(&log), Some("roundcube"));
    }

    #[tokio::test]
    async fn republishing_under_the_wrapper_only_updates_envelopes_script() {
        let (attempt, log) = publish_against(
            server_scripts(&[
                ("roundcube", false),
                ("envelope-rules", false),
                ("envelope-rules-wrapper", true),
            ]),
            true,
            ExistingScript::Refuse,
        )
        .await;

        let outcome = attempt.result.expect("publish");
        assert_eq!(outcome.activation, Activation::KeepWrapper);
        assert_eq!(outcome.active_script, "envelope-rules-wrapper");
        assert_eq!(log.commands, ["LISTSCRIPTS", "PUTSCRIPT envelope-rules"]);
        assert_eq!(active(&log), Some("envelope-rules-wrapper"));
        assert_kept(&log, "roundcube");
    }

    #[tokio::test]
    async fn no_active_script_or_envelopes_own_is_simply_activated() {
        for (scripts, existing) in [
            (server_scripts(&[]), ExistingScript::Refuse),
            (
                server_scripts(&[("roundcube", false)]),
                ExistingScript::Refuse,
            ),
            (
                server_scripts(&[("envelope-rules", true)]),
                ExistingScript::Refuse,
            ),
            (
                server_scripts(&[("envelope-rules", true)]),
                ExistingScript::Replace("roundcube".to_string()),
            ),
        ] {
            let (attempt, log) = publish_against(scripts.clone(), true, existing).await;
            let outcome = attempt.result.expect("publish");
            assert_eq!(outcome.activation, Activation::Activate, "{scripts:?}");
            assert_eq!(
                log.commands,
                [
                    "LISTSCRIPTS",
                    "PUTSCRIPT envelope-rules",
                    "SETACTIVE envelope-rules"
                ]
            );
        }
    }

    #[tokio::test]
    async fn list_scripts_reads_quoted_literal_and_utf8_names() {
        let (client, server) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut read = BufReader::new(read);
            let mut command = String::new();
            read.read_line(&mut command).await.unwrap();
            write
                .write_all(
                    "\"summer_script\"\r\n{13}\r\nclever\"script\r\n\"R\u{e8}gles \\\"perso\\\"\" ACTIVE\r\n{6+}\r\nwinter\r\nOK \"Listscripts completed.\"\r\n"
                        .as_bytes(),
                )
                .await
                .unwrap();
            command
        });
        let mut stream = BufStream::new(client);

        let scripts = list_scripts(&mut stream, T).await.unwrap();

        assert_eq!(server.await.unwrap(), "LISTSCRIPTS\r\n");
        assert_eq!(
            scripts,
            server_scripts(&[
                ("summer_script", false),
                ("clever\"script", false),
                ("R\u{e8}gles \"perso\"", true),
                ("winter", false),
            ])
        );
    }

    #[test]
    fn wrapper_escapes_quotes_in_script_names() {
        let wrapper = wrapper_script("my \"old\" rules", "envelope-rules");
        assert!(
            wrapper.contains(r#"include :personal :optional "my \"old\" rules";"#),
            "{wrapper}"
        );
    }

    #[test]
    fn dry_run_plan_shows_what_happens_in_each_server_state() {
        let refuse = activation_plan("envelope-rules", &ExistingScript::Refuse);
        assert_eq!(refuse.on_another_active_script, "refuse");
        assert!(refuse.if_another_script_active.starts_with("refuse"));
        assert!(
            refuse
                .if_no_script_or_envelope_script_active
                .contains("make it the active script")
        );
        assert!(
            refuse
                .if_envelope_wrapper_active
                .contains("envelope-rules-wrapper")
        );

        let keep = activation_plan("envelope-rules", &ExistingScript::Keep);
        assert_eq!(keep.on_another_active_script, "keep_existing");
        assert!(
            keep.if_another_script_active.contains("Sieve include")
                && keep
                    .if_another_script_active
                    .contains("\"envelope-rules-wrapper\"")
                && keep
                    .if_another_script_active
                    .contains("Without include support: refuse"),
            "{}",
            keep.if_another_script_active
        );

        let replace = activation_plan(
            "envelope-rules",
            &ExistingScript::Replace("roundcube".to_string()),
        );
        assert_eq!(replace.on_another_active_script, "replace_active");
        assert!(
            replace
                .if_another_script_active
                .contains("switch \"roundcube\" off (it stays on the server)")
                && replace
                    .if_another_script_active
                    .contains("Any other script: refuse"),
            "{}",
            replace.if_another_script_active
        );

        for plan in [refuse, keep, replace] {
            assert!(!plan.deletes_scripts);
        }
    }
}
