use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const ENVELOPE_BIN_ENV: &str = "ENVELOPE_DESKTOP_BIN";
pub const ENVELOPE_PORT_ENV: &str = "ENVELOPE_DESKTOP_PORT";
pub const DASHBOARD_PATH: &str = "/";
// A stable, non-hashed file emitted by the SvelteKit build. Hashed bundle
// names change every build; this one does not, and serving it proves the
// embedded SPA assets are reachable rather than just the fallback shell.
pub const STATIC_ASSET_PATH: &str = "/_app/version.json";
pub const API_ROUTE_PATH: &str = "/api/messages/unified?limit=1";
pub const HEALTH_PATHS: &[&str] = &[DASHBOARD_PATH, STATIC_ASSET_PATH, API_ROUTE_PATH];
pub const OUTPUT_TAIL_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinarySource {
    Env,
    /// Binary bundled inside the app, next to this executable. The only source
    /// that works in a shipped bundle: CARGO_MANIFEST_DIR is a build-machine
    /// path, and a Finder-launched app gets PATH=/usr/bin:/bin:/usr/sbin:/sbin,
    /// which excludes ~/.local/bin, ~/.cargo/bin and Homebrew.
    Sidecar,
    RepoDebug,
    Path,
}

impl BinarySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => ENVELOPE_BIN_ENV,
            Self::Sidecar => "bundled sidecar",
            Self::RepoDebug => "repo target/debug/envelope",
            Self::Path => "PATH",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryCandidate {
    pub source: BinarySource,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryResolution {
    pub source: BinarySource,
    pub path: PathBuf,
    pub attempted: Vec<BinaryCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryResolutionError {
    pub attempted: Vec<BinaryCandidate>,
    pub message: String,
}

impl fmt::Display for BinaryResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSource {
    Env,
    FreeLocalhost,
}

impl PortSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => ENVELOPE_PORT_ENV,
            Self::FreeLocalhost => "free localhost port",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSelection {
    pub port: u16,
    pub source: PortSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSelectionError {
    pub message: String,
}

impl fmt::Display for PortSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthProbe {
    pub path: String,
    pub url: String,
    pub ok: bool,
    pub status_code: Option<u16>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    ServerNotStarted,
    /// Stopped on purpose from the tray or app menu.
    ServerStopped,
    ServerStartFailed,
    DashboardRouteFailure,
    StaticAssetFailure,
    ApiRouteFailure,
    DashboardReady,
}

impl Readiness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServerNotStarted => "server_not_started",
            Self::ServerStopped => "server_stopped",
            Self::ServerStartFailed => "server_start_failed",
            Self::DashboardRouteFailure => "dashboard_route_failure",
            Self::StaticAssetFailure => "static_asset_failure",
            Self::ApiRouteFailure => "api_route_failure",
            Self::DashboardReady => "dashboard_ready",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TailBuffer {
    limit: usize,
    bytes: Vec<u8>,
}

impl TailBuffer {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            bytes: Vec::new(),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        if self.limit == 0 {
            self.bytes.clear();
            return;
        }

        self.bytes.extend_from_slice(chunk);

        if self.bytes.len() > self.limit {
            let drop_len = self.bytes.len() - self.limit;
            self.bytes.drain(..drop_len);
        }
    }

    pub fn as_string(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

impl Default for TailBuffer {
    fn default() -> Self {
        Self::new(OUTPUT_TAIL_BYTES)
    }
}

pub fn resolve_binary(manifest_dir: &Path) -> Result<BinaryResolution, BinaryResolutionError> {
    resolve_binary_with_env(
        manifest_dir,
        current_exe_dir(),
        env::var_os(ENVELOPE_BIN_ENV),
        env::var_os("PATH"),
    )
}

/// Directory holding this executable. Tauri places `externalBin` entries here
/// (`Contents/MacOS/` inside a .app), so it is where the bundled server lives.
pub fn current_exe_dir() -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// The default dashboard port, where an already-installed Envelope usually
/// serves. Used to offer a working way out when this app's own server cannot
/// start.
pub const DEFAULT_DASHBOARD_PORT: u16 = 3141;

/// What went wrong at startup, in terms a person can act on.
///
/// The shell used to surface "exited with exit status: 1" and a command line,
/// which tells you nothing you can do. Every failure the server can plausibly
/// hit at startup is recognised here and paired with a remedy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StartupGuidance {
    /// Stable identifier so the frontend can offer the right action.
    pub kind: &'static str,
    pub title: String,
    pub explanation: String,
    pub remedy: String,
}

/// Recognise a startup failure from what the server printed before exiting.
pub fn classify_startup_failure(output: &str) -> Option<StartupGuidance> {
    let text = output.to_ascii_lowercase();

    if text.contains("databasetoofarahead") || text.contains("database too far ahead") {
        return Some(StartupGuidance {
            kind: "schema_too_new",
            title: "Your mailbox is newer than this app".to_string(),
            explanation:
                "The database was upgraded by a newer version of Envelope, and the copy \
                 bundled with this app does not understand the new format. Nothing is \
                 damaged — this app simply cannot read it yet."
                    .to_string(),
            remedy:
                "Update Envelope Desktop. Until then your newer Envelope can still serve \
                 the dashboard."
                    .to_string(),
        });
    }

    if text.contains("no passphrase available") || text.contains("decryption error") {
        return Some(StartupGuidance {
            kind: "credentials_locked",
            title: "Your credentials are locked".to_string(),
            explanation:
                "The server could not unlock your stored account passwords, and it has no \
                 terminal to ask for a passphrase."
                    .to_string(),
            remedy:
                "Create a passphrase file, or set ENVELOPE_MASTER_PASSPHRASE_FILE, then \
                 start the server again. `envelope doctor` reports what is missing."
                    .to_string(),
        });
    }

    if text.contains("address already in use") {
        return Some(StartupGuidance {
            kind: "port_in_use",
            title: "Something already has that port".to_string(),
            explanation:
                "Another process took the port between this app choosing it and the server \
                 binding it."
                    .to_string(),
            remedy: "Starting the server again picks a new port.".to_string(),
        });
    }

    if text.contains("refusing to bind") || text.contains("refusing --no-auth") {
        return Some(StartupGuidance {
            kind: "auth_refused",
            title: "The server refused to start unauthenticated".to_string(),
            explanation:
                "Envelope will not serve mailboxes without a credential on a port that \
                 something else might be exposing."
                    .to_string(),
            remedy:
                "This app runs on a private port and should never hit this; the message \
                 below is the server's own explanation."
                    .to_string(),
        });
    }

    None
}

/// Env var naming the file that holds the credential master passphrase.
pub const PASSPHRASE_FILE_ENV: &str = "ENVELOPE_MASTER_PASSPHRASE_FILE";
/// Env var holding a raw master key, which takes precedence over the file.
pub const MASTER_KEY_ENV: &str = "ENVELOPE_MASTER_KEY";

/// Places a credential passphrase file is conventionally kept, most specific
/// first.
///
/// A GUI launch inherits none of the shell environment, so without one of these
/// the server has no way to unlock stored credentials and cannot prompt for one
/// either — every account fails with "no passphrase available and no
/// interactive terminal".
///
/// Two names are probed because two exist in the wild: docs/credential-backends.md
/// prescribes `<config>/envelope-email/passphrase`, while installed launcher
/// scripts export `envelope-master-passphrase`. Probing only one leaves half of
/// the users with a shell that cannot read their mail. The literal `~/.config`
/// path is included for macOS, where the documented command writes there but
/// `config_dir()` resolves to Library/Application Support.
pub fn default_passphrase_files() -> Vec<PathBuf> {
    let mut out = Vec::new();

    if let Some(dir) = dirs_next::config_dir() {
        let app = dir.join("envelope-email");
        out.push(app.join("envelope-master-passphrase"));
        out.push(app.join("passphrase"));
    }

    if let Some(home) = dirs_next::home_dir() {
        let dotconfig = home.join(".config").join("envelope-email");
        for name in ["envelope-master-passphrase", "passphrase"] {
            let candidate = dotconfig.join(name);
            if !out.contains(&candidate) {
                out.push(candidate);
            }
        }
    }

    out
}

/// Passphrase file to hand the server, or `None` to leave the environment
/// alone. Anything already set wins, as does a raw master key.
pub fn passphrase_file_for_spawn(
    existing_file: Option<OsString>,
    existing_key: Option<OsString>,
    candidates: Vec<PathBuf>,
) -> Option<PathBuf> {
    let already_configured = existing_file.is_some_and(|v| !v.is_empty())
        || existing_key.is_some_and(|v| !v.is_empty());

    if already_configured {
        None
    } else {
        candidates.into_iter().find(|path| path.is_file())
    }
}

/// Path the bundled server would occupy for a given executable directory.
pub fn sidecar_binary(exe_dir: &Path) -> PathBuf {
    exe_dir.join(executable_name("envelope"))
}

pub fn resolve_binary_with_env(
    manifest_dir: &Path,
    exe_dir: Option<PathBuf>,
    envelope_bin: Option<OsString>,
    path_env: Option<OsString>,
) -> Result<BinaryResolution, BinaryResolutionError> {
    if let Some(raw) = envelope_bin {
        if raw.as_os_str().is_empty() {
            return Err(BinaryResolutionError {
                attempted: vec![BinaryCandidate {
                    source: BinarySource::Env,
                    path: PathBuf::new(),
                }],
                message: format!("{ENVELOPE_BIN_ENV} is set but empty"),
            });
        }

        let requested = PathBuf::from(raw.clone());
        let resolved = if path_has_separator(&requested) || requested.is_absolute() {
            Some(requested.clone())
        } else {
            resolve_on_path(raw.as_os_str(), path_env.as_ref()).or(Some(requested.clone()))
        };

        let candidate = BinaryCandidate {
            source: BinarySource::Env,
            path: resolved.unwrap_or(requested),
        };

        return if is_runnable_file(&candidate.path) {
            Ok(BinaryResolution {
                source: candidate.source,
                path: candidate.path.clone(),
                attempted: vec![candidate],
            })
        } else {
            Err(BinaryResolutionError {
                attempted: vec![candidate],
                message: format!(
                    "{ENVELOPE_BIN_ENV} does not point to an executable Envelope binary"
                ),
            })
        };
    }

    let mut attempted = Vec::new();

    // Checked before the repo build so a shipped bundle never depends on the
    // build machine's layout or on the launching process's PATH.
    if let Some(dir) = exe_dir.as_deref() {
        let sidecar = sidecar_binary(dir);
        attempted.push(BinaryCandidate {
            source: BinarySource::Sidecar,
            path: sidecar.clone(),
        });

        if is_runnable_file(&sidecar) {
            return Ok(BinaryResolution {
                source: BinarySource::Sidecar,
                path: sidecar,
                attempted,
            });
        }
    }

    let repo_debug = repo_debug_binary_from_manifest(manifest_dir);
    attempted.push(BinaryCandidate {
        source: BinarySource::RepoDebug,
        path: repo_debug.clone(),
    });

    if is_runnable_file(&repo_debug) {
        return Ok(BinaryResolution {
            source: BinarySource::RepoDebug,
            path: repo_debug,
            attempted,
        });
    }

    if let Some(path_binary) =
        resolve_on_path(OsStr::new(&executable_name("envelope")), path_env.as_ref())
    {
        let candidate = BinaryCandidate {
            source: BinarySource::Path,
            path: path_binary,
        };
        attempted.push(candidate.clone());

        return Ok(BinaryResolution {
            source: BinarySource::Path,
            path: candidate.path,
            attempted,
        });
    }

    attempted.push(BinaryCandidate {
        source: BinarySource::Path,
        path: PathBuf::from(executable_name("envelope")),
    });

    Err(BinaryResolutionError {
        attempted,
        message: format!(
            "could not find Envelope binary via {ENVELOPE_BIN_ENV}, a bundled sidecar, repo target/debug/envelope, or PATH"
        ),
    })
}

pub fn repo_root_from_manifest(manifest_dir: &Path) -> PathBuf {
    manifest_dir
        .ancestors()
        .nth(3)
        .unwrap_or(manifest_dir)
        .to_path_buf()
}

pub fn repo_debug_binary_from_manifest(manifest_dir: &Path) -> PathBuf {
    repo_root_from_manifest(manifest_dir)
        .join("target")
        .join("debug")
        .join(executable_name("envelope"))
}

pub fn select_port() -> Result<PortSelection, PortSelectionError> {
    select_port_with_env(env::var(ENVELOPE_PORT_ENV).ok().as_deref())
}

pub fn select_port_with_env(
    port_override: Option<&str>,
) -> Result<PortSelection, PortSelectionError> {
    if let Some(raw) = port_override {
        let trimmed = raw.trim();

        if !trimmed.is_empty() {
            let port = trimmed.parse::<u16>().map_err(|_| PortSelectionError {
                message: format!("{ENVELOPE_PORT_ENV} must be an integer between 1 and 65535"),
            })?;

            if port == 0 {
                return Err(PortSelectionError {
                    message: format!("{ENVELOPE_PORT_ENV} must not be 0"),
                });
            }

            return Ok(PortSelection {
                port,
                source: PortSource::Env,
            });
        }
    }

    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|error| PortSelectionError {
            message: format!("failed to reserve a free localhost port: {error}"),
        })?;
    let port = listener
        .local_addr()
        .map_err(|error| PortSelectionError {
            message: format!("failed to inspect reserved localhost port: {error}"),
        })?
        .port();

    Ok(PortSelection {
        port,
        source: PortSource::FreeLocalhost,
    })
}

pub fn dashboard_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

pub fn health_url(port: u16, path: &str) -> String {
    let normalized = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };

    format!("http://127.0.0.1:{port}{normalized}")
}

pub fn command_preview(binary: &Path, port: u16) -> String {
    format!(
        "{} {}",
        shell_quote_path(binary),
        server_args(port).join(" ")
    )
}

pub fn server_args(port: u16) -> [String; 4] {
    [
        "serve".to_string(),
        // Private server on an ephemeral loopback port. A configured
        // dashboard.auth_token exists for the `tailscale serve` case and does
        // not apply here; inheriting it would 401 every API call, because the
        // dashboard UI has no way to present a bearer token. `serve` refuses
        // --no-auth on the fronted default port, so this cannot publish
        // anything.
        //
        // Background sweeps are deliberately NOT disabled. The dashboard has no
        // inline send path: POST /drafts/:id/send only queues a draft with a
        // send_after, and the SMTP transmission happens in the scheduled-send
        // sweep. Without it every message composed here reports "queued" and is
        // never sent. When another server already runs this home's background
        // jobs (the installed dashboard), `serve` leaves them to it and takes
        // them over if it stops, so the app never runs them twice.
        "--no-auth".to_string(),
        "--port".to_string(),
        port.to_string(),
    ]
}

pub fn check_all_health(port: u16, timeout: Duration) -> Vec<HealthProbe> {
    HEALTH_PATHS
        .iter()
        .map(|path| check_health(port, path, timeout))
        .collect()
}

pub fn all_health_ok(probes: &[HealthProbe]) -> bool {
    !probes.is_empty() && probes.iter().all(|probe| probe.ok)
}

pub fn classify_readiness(
    running: bool,
    probes: &[HealthProbe],
    startup_error: Option<&str>,
) -> Readiness {
    if startup_error
        .map(|error| {
            error.contains("failed to start")
                || error.contains("exited during startup")
                || error.contains("failed to inspect Envelope server during startup")
        })
        .unwrap_or(false)
    {
        return Readiness::ServerStartFailed;
    }

    if !running {
        return Readiness::ServerNotStarted;
    }

    if !probe_ok(probes, DASHBOARD_PATH) {
        return Readiness::DashboardRouteFailure;
    }

    if !probe_ok(probes, STATIC_ASSET_PATH) {
        return Readiness::StaticAssetFailure;
    }

    if !probe_ok(probes, API_ROUTE_PATH) {
        return Readiness::ApiRouteFailure;
    }

    Readiness::DashboardReady
}

pub fn redact_diagnostics_text(input: &str, home_dir: Option<&Path>) -> String {
    let without_home = redact_home_dir(input, home_dir);
    let without_local_ports = redact_localhost_ports(&without_home);
    let without_codes = redact_six_digit_codes(&without_local_ports);

    redact_sensitive_lines(&without_codes)
}

pub fn check_health(port: u16, path: &str, timeout: Duration) -> HealthProbe {
    let normalized = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let url = health_url(port, &normalized);
    let socket = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));

    let mut stream = match TcpStream::connect_timeout(&socket, timeout) {
        Ok(stream) => stream,
        Err(error) => {
            return HealthProbe {
                path: normalized,
                url,
                ok: false,
                status_code: None,
                error: Some(format!("connect failed: {error}")),
            };
        }
    };

    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let request = format!(
        "GET {normalized} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );

    if let Err(error) = stream.write_all(request.as_bytes()) {
        return HealthProbe {
            path: normalized,
            url,
            ok: false,
            status_code: None,
            error: Some(format!("request failed: {error}")),
        };
    }

    let mut response = String::new();
    let read_result = stream.take(8192).read_to_string(&mut response);

    if let Err(error) = read_result {
        return HealthProbe {
            path: normalized,
            url,
            ok: false,
            status_code: None,
            error: Some(format!("response read failed: {error}")),
        };
    }

    let status_code = response.lines().next().and_then(parse_status_code);
    let content_type = parse_header(&response, "content-type");
    let (ok, error) = evaluate_response(&normalized, status_code, content_type);

    HealthProbe {
        path: normalized,
        url,
        ok,
        status_code,
        error,
    }
}

/// Decide whether a probe response counts as healthy.
///
/// A 200 alone proves nothing: the dashboard answers unknown paths with the SPA
/// shell, so a deleted asset still returns `200 text/html` and a status-only
/// check passes silently. Where the media type is the whole point of the probe,
/// it is checked too.
pub fn evaluate_response(
    path: &str,
    status_code: Option<u16>,
    content_type: Option<&str>,
) -> (bool, Option<String>) {
    let Some(code) = status_code else {
        return (false, Some("missing HTTP status line".to_string()));
    };

    if !(200..400).contains(&code) {
        return (false, None);
    }

    let Some(want) = expected_content_type(path) else {
        return (true, None);
    };

    match content_type {
        Some(got) if got.to_ascii_lowercase().contains(want) => (true, None),
        Some(got) => (
            false,
            Some(format!(
                "{path} returned {code} but a {want} content-type was required; got {got}"
            )),
        ),
        None => (
            false,
            Some(format!(
                "{path} returned {code} but a {want} content-type was required; got none"
            )),
        ),
    }
}

/// Media type a probe must see, keyed by path. `None` means status-only.
fn expected_content_type(path: &str) -> Option<&'static str> {
    match path {
        STATIC_ASSET_PATH => Some("json"),
        API_ROUTE_PATH => Some("json"),
        _ => None,
    }
}

fn parse_header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    response
        .lines()
        .take_while(|line| !line.is_empty())
        .skip(1)
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim())
        })
}

fn resolve_on_path(binary_name: &OsStr, path_env: Option<&OsString>) -> Option<PathBuf> {
    let path_env = path_env?;

    for dir in env::split_paths(path_env) {
        let candidate = dir.join(binary_name);

        if is_runnable_file(&candidate) {
            return Some(candidate);
        }
    }

    None
}

pub fn parse_status_code(line: &str) -> Option<u16> {
    let mut parts = line.split_whitespace();
    let protocol = parts.next()?;

    if !protocol.starts_with("HTTP/") {
        return None;
    }

    parts.next()?.parse().ok()
}

fn probe_ok(probes: &[HealthProbe], path: &str) -> bool {
    probes
        .iter()
        .find(|probe| probe.path == path)
        .map(|probe| probe.ok)
        .unwrap_or(false)
}

fn redact_home_dir(input: &str, home_dir: Option<&Path>) -> String {
    let Some(home_dir) = home_dir else {
        return input.to_string();
    };

    let home = home_dir.to_string_lossy();

    if home.is_empty() || home == "/" {
        input.to_string()
    } else {
        input.replace(home.as_ref(), "~")
    }
}

fn redact_localhost_ports(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut index = 0;

    while index < input.len() {
        let remaining = &input[index..];
        let Some(prefix) = local_address_prefix(remaining) else {
            if let Some(char) = remaining.chars().next() {
                output.push(char);
                index += char.len_utf8();
            }
            continue;
        };

        output.push_str(prefix);
        output.push_str(":<local-port>");
        index += prefix.len() + 1;

        while input[index..]
            .chars()
            .next()
            .map(|char| char.is_ascii_digit())
            .unwrap_or(false)
        {
            index += 1;
        }
    }

    output
}

fn local_address_prefix(input: &str) -> Option<&'static str> {
    for prefix in ["http://127.0.0.1", "https://127.0.0.1", "127.0.0.1"] {
        if input.starts_with(prefix)
            && input[prefix.len()..]
                .chars()
                .next()
                .map(|char| char == ':')
                .unwrap_or(false)
        {
            return Some(prefix);
        }
    }

    None
}

fn redact_six_digit_codes(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut previous_was_alphanumeric = false;

    while let Some(char) = chars.next() {
        if !char.is_ascii_digit() {
            previous_was_alphanumeric = char.is_ascii_alphanumeric();
            output.push(char);
            continue;
        }

        let mut run = String::from(char);

        while chars
            .peek()
            .map(|peek| peek.is_ascii_digit())
            .unwrap_or(false)
        {
            run.push(chars.next().expect("peeked digit"));
        }

        let next_is_alphanumeric = chars
            .peek()
            .map(|peek| peek.is_ascii_alphanumeric())
            .unwrap_or(false);

        if run.len() == 6 && !previous_was_alphanumeric && !next_is_alphanumeric {
            output.push_str("<redacted-code>");
        } else {
            output.push_str(&run);
        }

        previous_was_alphanumeric = next_is_alphanumeric;
    }

    output
}

fn redact_sensitive_lines(input: &str) -> String {
    let mut output = String::with_capacity(input.len());

    for line in input.split_inclusive('\n') {
        let (content, newline) = line
            .strip_suffix('\n')
            .map(|content| (content, "\n"))
            .unwrap_or((line, ""));

        if contains_sensitive_label(content) {
            output.push_str("<redacted sensitive log line>");
        } else {
            output.push_str(content);
        }

        output.push_str(newline);
    }

    output
}

fn contains_sensitive_label(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();

    [
        "authorization",
        "cookie",
        "password",
        "passphrase",
        "secret",
        "token",
        "api_key",
        "apikey",
        "otp",
    ]
    .iter()
    .any(|label| lower.contains(label))
}

fn executable_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn path_has_separator(path: &Path) -> bool {
    path.components().count() > 1
}

fn shell_quote_path(path: &Path) -> String {
    let raw = path.to_string_lossy();

    if raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':'))
    {
        raw.into_owned()
    } else {
        format!("'{}'", raw.replace('\'', "'\\''"))
    }
}

fn is_runnable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };

    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn repo_debug_binary_uses_repo_target_from_tauri_manifest() {
        let manifest_dir = Path::new("/repo/apps/desktop/src-tauri");

        assert_eq!(
            repo_debug_binary_from_manifest(manifest_dir),
            PathBuf::from("/repo/target/debug").join(executable_name("envelope"))
        );
    }

    #[test]
    fn port_override_parses_env_value() {
        let selection = select_port_with_env(Some("4401")).unwrap();

        assert_eq!(selection.port, 4401);
        assert_eq!(selection.source, PortSource::Env);
    }

    #[test]
    fn port_override_rejects_zero() {
        let error = select_port_with_env(Some("0")).unwrap_err();

        assert!(error.message.contains("must not be 0"));
    }

    #[test]
    fn health_url_normalizes_paths() {
        assert_eq!(dashboard_url(3141), "http://127.0.0.1:3141/");
        assert_eq!(
            health_url(3141, "static/dashboard.js"),
            "http://127.0.0.1:3141/static/dashboard.js"
        );
        assert_eq!(
            health_url(3141, "/api/messages/unified?limit=1"),
            "http://127.0.0.1:3141/api/messages/unified?limit=1"
        );
    }

    #[test]
    fn command_preview_quotes_paths_with_spaces() {
        assert_eq!(
            command_preview(Path::new("/tmp/Envelope Debug/envelope"), 3141),
            "'/tmp/Envelope Debug/envelope' serve --no-auth --port 3141"
        );
    }

    #[test]
    fn server_args_run_sweeps_so_queued_mail_actually_sends() {
        // --no-background-sweeps must NOT appear: the dashboard only queues a
        // send, and the sweep is what transmits it.
        let args = server_args(4401);

        assert_eq!(
            args,
            [
                "serve".to_string(),
                "--no-auth".to_string(),
                "--port".to_string(),
                "4401".to_string(),
            ]
        );
        assert!(!args.contains(&"--no-background-sweeps".to_string()));
    }

    #[test]
    fn spa_shell_served_for_a_missing_static_asset_is_not_healthy() {
        // The regression this guards: the dashboard answers unknown paths with
        // the SPA shell, so /static/dashboard.js returned 200 text/html even
        // after the asset was gone, and the probe passed anyway.
        let (ok, error) = evaluate_response(STATIC_ASSET_PATH, Some(200), Some("text/html"));

        assert!(!ok, "200 text/html must not satisfy a static-asset probe");
        assert!(error.unwrap().contains("json"));
    }

    #[test]
    fn real_static_asset_is_healthy() {
        let (ok, error) = evaluate_response(
            STATIC_ASSET_PATH,
            Some(200),
            Some("application/json; charset=utf-8"),
        );

        assert!(ok);
        assert!(error.is_none());
    }

    #[test]
    fn api_route_must_answer_json() {
        let (html_ok, _) = evaluate_response(API_ROUTE_PATH, Some(200), Some("text/html"));
        let (json_ok, _) = evaluate_response(API_ROUTE_PATH, Some(200), Some("application/json"));

        assert!(!html_ok);
        assert!(json_ok);
    }

    #[test]
    fn dashboard_root_is_status_only() {
        // The SPA shell is the correct answer at "/", so no media-type demand.
        let (ok, error) = evaluate_response(DASHBOARD_PATH, Some(200), Some("text/html"));

        assert!(ok);
        assert!(error.is_none());
    }

    #[test]
    fn unauthorized_api_response_is_not_healthy() {
        let (ok, _) = evaluate_response(API_ROUTE_PATH, Some(401), Some("application/json"));

        assert!(!ok, "401 must fail the probe");
    }

    #[test]
    fn missing_status_line_is_not_healthy() {
        let (ok, error) = evaluate_response(DASHBOARD_PATH, None, None);

        assert!(!ok);
        assert_eq!(error.unwrap(), "missing HTTP status line");
    }

    #[test]
    fn parse_header_is_case_insensitive_and_stops_at_the_body() {
        let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\nContent-Type: not-a-header";

        assert_eq!(
            parse_header(response, "content-type"),
            Some("application/json")
        );
        assert_eq!(parse_header(response, "missing"), None);
    }

    #[test]
    fn binary_resolution_prefers_sidecar_over_repo_and_path() {
        let temp_dir = unique_temp_dir("sidecar-bin");
        fs::create_dir_all(&temp_dir).unwrap();
        let binary = sidecar_binary(&temp_dir);
        File::create(&binary).unwrap();
        make_executable(&binary);

        let resolution = resolve_binary_with_env(
            Path::new("/repo/apps/desktop/src-tauri"),
            Some(temp_dir.clone()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(resolution.source, BinarySource::Sidecar);
        assert_eq!(resolution.path, binary);
    }

    #[test]
    fn missing_sidecar_is_recorded_and_falls_through() {
        // A bundle with no sidecar must say so in the diagnostics rather than
        // failing with only PATH mentioned.
        let empty = unique_temp_dir("no-sidecar");
        fs::create_dir_all(&empty).unwrap();

        let error = resolve_binary_with_env(
            Path::new("/repo/apps/desktop/src-tauri"),
            Some(empty.clone()),
            None,
            None,
        )
        .unwrap_err();

        assert_eq!(error.attempted[0].source, BinarySource::Sidecar);
        assert_eq!(error.attempted[0].path, sidecar_binary(&empty));
        assert!(error.message.contains("sidecar"));
    }

    #[test]
    fn spawn_supplies_the_default_passphrase_file_when_env_is_bare() {
        // The GUI case: no shell environment, so the server would otherwise have
        // no key source and no terminal to prompt on.
        let dir = unique_temp_dir("pass-default");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("envelope-master-passphrase");
        File::create(&file).unwrap();

        assert_eq!(
            passphrase_file_for_spawn(None, None, vec![file.clone()]),
            Some(file)
        );
    }

    #[test]
    fn an_existing_passphrase_file_setting_is_left_alone() {
        let dir = unique_temp_dir("pass-existing");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("envelope-master-passphrase");
        File::create(&file).unwrap();

        assert_eq!(
            passphrase_file_for_spawn(
                Some(OsString::from("/somewhere/else")),
                None,
                vec![file]
            ),
            None
        );
    }

    #[test]
    fn a_raw_master_key_wins_over_the_default_file() {
        let dir = unique_temp_dir("pass-key");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("envelope-master-passphrase");
        File::create(&file).unwrap();

        assert_eq!(
            passphrase_file_for_spawn(None, Some(OsString::from("raw-key")), vec![file]),
            None
        );
    }

    #[test]
    fn an_empty_setting_does_not_count_as_configured() {
        let dir = unique_temp_dir("pass-empty");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("envelope-master-passphrase");
        File::create(&file).unwrap();

        assert_eq!(
            passphrase_file_for_spawn(
                Some(OsString::new()),
                Some(OsString::new()),
                vec![file.clone()]
            ),
            Some(file)
        );
    }

    #[test]
    fn a_missing_default_file_sets_nothing() {
        // Never point the server at a file that is not there; let it report its
        // own credential error rather than inventing a path.
        let dir = unique_temp_dir("pass-absent");
        assert_eq!(
            passphrase_file_for_spawn(None, None, vec![dir.join("envelope-master-passphrase")]),
            None
        );
    }

    #[test]
    fn the_documented_passphrase_name_is_also_accepted() {
        // docs/credential-backends.md prescribes `passphrase`; installed
        // launcher scripts use `envelope-master-passphrase`. Both must work.
        let dir = unique_temp_dir("pass-documented");
        fs::create_dir_all(&dir).unwrap();
        let documented = dir.join("passphrase");
        File::create(&documented).unwrap();

        assert_eq!(
            passphrase_file_for_spawn(
                None,
                None,
                vec![dir.join("envelope-master-passphrase"), documented.clone()],
            ),
            Some(documented)
        );
    }

    #[test]
    fn the_first_existing_candidate_wins() {
        let dir = unique_temp_dir("pass-order");
        fs::create_dir_all(&dir).unwrap();
        let first = dir.join("envelope-master-passphrase");
        let second = dir.join("passphrase");
        File::create(&first).unwrap();
        File::create(&second).unwrap();

        assert_eq!(
            passphrase_file_for_spawn(None, None, vec![first.clone(), second]),
            Some(first)
        );
    }

    #[test]
    fn a_newer_schema_is_explained_rather_than_dumped() {
        let g = classify_startup_failure(
            "Error: migration error: rusqlite_migrate error: MigrationDefinition(DatabaseTooFarAhead)",
        )
        .expect("schema skew must be recognised");

        assert_eq!(g.kind, "schema_too_new");
        assert!(g.title.to_lowercase().contains("newer"));
        // The remedy must not be the raw error or a command line.
        assert!(!g.remedy.contains("rusqlite"));
    }

    #[test]
    fn locked_credentials_are_recognised() {
        let g = classify_startup_failure(
            "IMAP: credential store error: configuration error: no passphrase available and \
             no interactive terminal to prompt for one.",
        )
        .expect("locked credentials must be recognised");

        assert_eq!(g.kind, "credentials_locked");
    }

    #[test]
    fn a_taken_port_is_recognised() {
        let g = classify_startup_failure("Error: Address already in use (os error 48)")
            .expect("port conflict must be recognised");

        assert_eq!(g.kind, "port_in_use");
    }

    #[test]
    fn an_auth_refusal_is_recognised() {
        let g = classify_startup_failure(
            "Error: refusing --no-auth on the default dashboard port (3141).",
        )
        .expect("auth refusal must be recognised");

        assert_eq!(g.kind, "auth_refused");
    }

    #[test]
    fn an_unrecognised_failure_yields_no_false_guidance() {
        // Better to show the raw output than to invent a wrong remedy.
        assert!(classify_startup_failure("Error: something nobody predicted").is_none());
    }

    #[test]
    fn tail_buffer_keeps_last_bytes() {
        let mut tail = TailBuffer::new(5);

        tail.push(b"hello");
        tail.push(b" world");

        assert_eq!(tail.as_string(), "world");
    }

    #[test]
    fn readiness_distinguishes_static_and_api_failures() {
        let probes = vec![
            probe(DASHBOARD_PATH, true),
            probe(STATIC_ASSET_PATH, false),
            probe(API_ROUTE_PATH, true),
        ];

        assert_eq!(
            classify_readiness(true, &probes, None),
            Readiness::StaticAssetFailure
        );

        let probes = vec![
            probe(DASHBOARD_PATH, true),
            probe(STATIC_ASSET_PATH, true),
            probe(API_ROUTE_PATH, false),
        ];

        assert_eq!(
            classify_readiness(true, &probes, None),
            Readiness::ApiRouteFailure
        );
    }

    #[test]
    fn readiness_reports_dashboard_ready_when_all_probes_pass() {
        let probes = vec![
            probe(DASHBOARD_PATH, true),
            probe(STATIC_ASSET_PATH, true),
            probe(API_ROUTE_PATH, true),
        ];

        assert_eq!(
            classify_readiness(true, &probes, None),
            Readiness::DashboardReady
        );
    }

    #[test]
    fn diagnostics_redaction_removes_local_noise_and_secret_lines() {
        let input = "\
/Users/someone/project envelope
http://127.0.0.1:44123/
code 123456
Authorization: Bearer abc
normal line
";

        let redacted = redact_diagnostics_text(input, Some(Path::new("/Users/someone")));

        assert!(redacted.contains("~/project envelope"));
        assert!(redacted.contains("http://127.0.0.1:<local-port>/"));
        assert!(redacted.contains("<redacted-code>"));
        assert!(redacted.contains("<redacted sensitive log line>"));
        assert!(redacted.contains("normal line"));
        assert!(!redacted.contains("Bearer abc"));
        assert!(!redacted.contains("123456"));
        assert!(!redacted.contains("/Users/someone"));
    }

    #[test]
    fn binary_resolution_prefers_env_override() {
        let temp_dir = unique_temp_dir("env-bin");
        fs::create_dir_all(&temp_dir).unwrap();
        let binary = temp_dir.join(executable_name("envelope"));
        File::create(&binary).unwrap();
        make_executable(&binary);

        let resolution = resolve_binary_with_env(
            Path::new("/repo/apps/desktop/src-tauri"),
            None,
            Some(binary.clone().into_os_string()),
            None,
        )
        .unwrap();

        assert_eq!(resolution.source, BinarySource::Env);
        assert_eq!(resolution.path, binary);

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn binary_resolution_falls_back_to_path() {
        let temp_dir = unique_temp_dir("path-bin");
        fs::create_dir_all(&temp_dir).unwrap();
        let binary = temp_dir.join(executable_name("envelope"));
        File::create(&binary).unwrap();
        make_executable(&binary);

        let path_env = env::join_paths([temp_dir.as_path()]).unwrap();
        let resolution = resolve_binary_with_env(
            Path::new("/repo/apps/desktop/src-tauri"),
            None,
            None,
            Some(path_env),
        )
        .unwrap();

        assert_eq!(resolution.source, BinarySource::Path);
        assert_eq!(resolution.path, binary);

        let _ = fs::remove_dir_all(temp_dir);
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "envelope-desktop-supervisor-{name}-{}",
            std::process::id()
        ))
    }

    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }

        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }

    fn probe(path: &str, ok: bool) -> HealthProbe {
        HealthProbe {
            path: path.to_string(),
            url: health_url(3141, path),
            ok,
            status_code: ok.then_some(200),
            error: if ok { None } else { Some("failed".to_string()) },
        }
    }
}
