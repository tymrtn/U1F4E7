mod alerts;
mod supervisor;
mod tray;

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use supervisor::{
    BinaryCandidate, BinaryResolutionError, DEFAULT_DASHBOARD_PORT, HealthProbe, MASTER_KEY_ENV,
    PASSPHRASE_FILE_ENV, PortSelectionError, Readiness, StartupGuidance, TailBuffer, all_health_ok,
    check_all_health, classify_readiness, classify_startup_failure, command_preview,
    dashboard_url, default_passphrase_files, passphrase_file_for_spawn, redact_diagnostics_text,
    resolve_binary, select_port, server_args,
};
use tauri::{
    Emitter, Manager, RunEvent, WindowEvent,
    menu::{Menu, MenuItem, Submenu},
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(250);
const STATUS_HEALTH_TIMEOUT: Duration = Duration::from_millis(800);
const MENU_OPEN_DASHBOARD: &str = "open_dashboard";
const MENU_OPEN_BROWSER: &str = "open_browser";
const MENU_COPY_DIAGNOSTICS: &str = "copy_diagnostics";
const MENU_RESTART_SERVER: &str = "restart_server";
const MENU_QUIT: &str = "quit";
const EVENT_STATUS: &str = "desktop-status";
const EVENT_OPEN_DASHBOARD: &str = "desktop-open-dashboard";
const EVENT_COPY_RESULT: &str = "desktop-diagnostics-copied";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopStatus {
    readiness: String,
    running: bool,
    healthy: bool,
    dashboard_url: Option<String>,
    binary_path: Option<String>,
    binary_source: Option<String>,
    binary_attempts: Vec<BinaryAttemptDto>,
    port: Option<u16>,
    port_source: Option<String>,
    last_health_check_at: Option<String>,
    health_checks: Vec<HealthCheckDto>,
    stdout_tail: String,
    stderr_tail: String,
    error: Option<String>,
    recommended_command: Option<String>,
    /// Plain-language explanation and remedy when the failure is recognised.
    guidance: Option<StartupGuidance>,
    /// A healthy Envelope already serving elsewhere, offered as a way through
    /// when this app's own server cannot start.
    existing_server_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthCheckDto {
    path: String,
    url: String,
    ok: bool,
    status_code: Option<u16>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BinaryAttemptDto {
    source: String,
    path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CopyDiagnosticsResult {
    status: DesktopStatus,
    diagnostics: String,
    copied: bool,
    error: Option<String>,
}

struct DesktopState {
    manifest_dir: PathBuf,
    inner: Mutex<SupervisorSlot>,
}

enum SupervisorSlot {
    Empty,
    Running(ManagedServer),
    Failed(DesktopStatus),
    /// The user stopped the server. Kept apart from `Empty` so the window's
    /// status poll does not start it again behind their back.
    Stopped,
}

struct ManagedServer {
    child: Child,
    binary_path: PathBuf,
    binary_source: String,
    port: u16,
    port_source: String,
    health_checks: Vec<HealthProbe>,
    startup_error: Option<String>,
    last_health_check_at: Option<String>,
    stdout_tail: Arc<Mutex<TailBuffer>>,
    stderr_tail: Arc<Mutex<TailBuffer>>,
}

impl DesktopState {
    fn new(manifest_dir: PathBuf) -> Self {
        Self {
            manifest_dir,
            inner: Mutex::new(SupervisorSlot::Empty),
        }
    }

    fn ensure_started(&self) -> DesktopStatus {
        let mut slot = self
            .inner
            .lock()
            .expect("desktop supervisor mutex poisoned");

        if matches!(*slot, SupervisorSlot::Empty) {
            *slot = self.spawn_slot();
        }

        match &mut *slot {
            SupervisorSlot::Empty => unreachable!("empty supervisor slot after startup"),
            SupervisorSlot::Running(server) => server.status(),
            SupervisorSlot::Failed(status) => status.clone(),
            SupervisorSlot::Stopped => stopped_status(),
        }
    }

    /// Current status without starting anything. `None` before the first start
    /// and during exit, when spawning a server would leave it orphaned.
    fn peek(&self) -> Option<DesktopStatus> {
        let mut slot = self
            .inner
            .lock()
            .expect("desktop supervisor mutex poisoned");

        match &mut *slot {
            SupervisorSlot::Empty => None,
            SupervisorSlot::Running(server) => Some(server.status()),
            SupervisorSlot::Failed(status) => Some(status.clone()),
            SupervisorSlot::Stopped => Some(stopped_status()),
        }
    }

    /// Whether the server process is alive, without probing its health.
    fn server_running(&self) -> bool {
        let mut slot = self
            .inner
            .lock()
            .expect("desktop supervisor mutex poisoned");

        match &mut *slot {
            SupervisorSlot::Running(server) => matches!(server.child.try_wait(), Ok(None)),
            _ => false,
        }
    }

    fn stop(&self) -> DesktopStatus {
        let mut slot = self
            .inner
            .lock()
            .expect("desktop supervisor mutex poisoned");

        if let SupervisorSlot::Running(server) = &mut *slot {
            server.stop();
        }

        *slot = SupervisorSlot::Stopped;
        stopped_status()
    }

    fn restart(&self) -> DesktopStatus {
        let mut slot = self
            .inner
            .lock()
            .expect("desktop supervisor mutex poisoned");

        if let SupervisorSlot::Running(server) = &mut *slot {
            server.stop();
        }

        *slot = self.spawn_slot();

        match &mut *slot {
            SupervisorSlot::Empty | SupervisorSlot::Stopped => {
                unreachable!("no server in the supervisor slot after restart")
            }
            SupervisorSlot::Running(server) => server.status(),
            SupervisorSlot::Failed(status) => status.clone(),
        }
    }

    /// Stop the supervised server and empty the slot.
    ///
    /// ManagedServer has a Drop impl, but relying on it leaks the server:
    /// Tauri tears the app down through `process::exit()`, which does not run
    /// destructors for managed state. The orphaned server keeps the mail
    /// database open and its port bound, one per launch. This must therefore be
    /// called explicitly from the exit event.
    fn shutdown(&self) {
        // A poisoned mutex must not stop us from killing the child.
        let mut slot = match self.inner.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let SupervisorSlot::Running(server) = &mut *slot {
            server.stop();
        }

        *slot = SupervisorSlot::Empty;
    }

    fn spawn_slot(&self) -> SupervisorSlot {
        let binary = match resolve_binary(&self.manifest_dir) {
            Ok(binary) => binary,
            Err(error) => return SupervisorSlot::Failed(status_from_binary_error(error)),
        };

        let port = match select_port() {
            Ok(port) => port,
            Err(error) => {
                let binary_path = binary.path.clone();
                let binary_source = binary.source.as_str().to_string();

                return SupervisorSlot::Failed(status_from_port_error(
                    error,
                    Some(binary_path),
                    Some(binary_source),
                ));
            }
        };

        let stdout_tail = Arc::new(Mutex::new(TailBuffer::default()));
        let stderr_tail = Arc::new(Mutex::new(TailBuffer::default()));

        let mut command = Command::new(&binary.path);
        command.args(server_args(port.port));

        // A GUI launch inherits no shell environment, so the server would have
        // no passphrase to unlock stored credentials and no terminal to prompt
        // on — every account fails to connect. Point it at the default file
        // unless the environment already specifies a key source.
        if let Some(path) = passphrase_file_for_spawn(
            std::env::var_os(PASSPHRASE_FILE_ENV),
            std::env::var_os(MASTER_KEY_ENV),
            default_passphrase_files(),
        ) {
            command.env(PASSPHRASE_FILE_ENV, path);
        }

        let mut child = match command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let binary_path = binary.path.clone();
                let binary_source = binary.source.as_str().to_string();

                return SupervisorSlot::Failed(status_from_spawn_error(
                    error,
                    binary_path,
                    binary_source,
                    port.port,
                    port.source.as_str().to_string(),
                ));
            }
        };

        if let Some(stdout) = child.stdout.take() {
            spawn_tail_reader(stdout, Arc::clone(&stdout_tail));
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_tail_reader(stderr, Arc::clone(&stderr_tail));
        }

        let (health_checks, startup_error, last_health_check_at) =
            wait_for_child_health(&mut child, port.port);

        SupervisorSlot::Running(ManagedServer {
            child,
            binary_path: binary.path,
            binary_source: binary.source.as_str().to_string(),
            port: port.port,
            port_source: port.source.as_str().to_string(),
            health_checks,
            startup_error,
            last_health_check_at,
            stdout_tail,
            stderr_tail,
        })
    }
}

impl ManagedServer {
    fn status(&mut self) -> DesktopStatus {
        let mut running = true;
        let mut error = self.startup_error.clone();

        match self.child.try_wait() {
            Ok(Some(status)) => {
                running = false;
                error = Some(format!("Envelope server exited with {status}"));
            }
            Ok(None) => {
                self.health_checks = check_all_health(self.port, STATUS_HEALTH_TIMEOUT);
                self.last_health_check_at = Some(utc_timestamp_now());
            }
            Err(wait_error) => {
                running = false;
                error = Some(format!(
                    "failed to inspect Envelope server process: {wait_error}"
                ));
            }
        }

        let healthy = running && all_health_ok(&self.health_checks);

        if healthy {
            self.startup_error = None;
            error = None;
        }

        if running && !healthy && error.is_none() {
            error = Some("Envelope server is running, but one or more health checks failed".into());
        }

        let readiness = classify_readiness(running, &self.health_checks, error.as_deref());

        DesktopStatus {
            readiness: readiness.as_str().to_string(),
            running,
            healthy,
            dashboard_url: Some(dashboard_url(self.port)),
            binary_path: Some(self.binary_path.display().to_string()),
            binary_source: Some(self.binary_source.clone()),
            binary_attempts: vec![BinaryAttemptDto {
                source: self.binary_source.clone(),
                path: self.binary_path.display().to_string(),
            }],
            port: Some(self.port),
            port_source: Some(self.port_source.clone()),
            last_health_check_at: self.last_health_check_at.clone(),
            health_checks: health_dtos(&self.health_checks),
            stdout_tail: tail_to_string(&self.stdout_tail),
            stderr_tail: tail_to_string(&self.stderr_tail),
            error,
            recommended_command: Some(command_preview(&self.binary_path, self.port)),
            guidance: classify_startup_failure(&combined_output(
                &self.stdout_tail,
                &self.stderr_tail,
            )),
            existing_server_url: existing_dashboard_url(),
        }
    }

    fn stop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            Err(_) => {}
        }
    }
}

impl Drop for ManagedServer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[tauri::command]
fn desktop_status(state: tauri::State<'_, DesktopState>) -> DesktopStatus {
    state.ensure_started()
}

#[tauri::command]
fn restart_server(state: tauri::State<'_, DesktopState>) -> DesktopStatus {
    state.restart()
}

#[tauri::command]
fn open_dashboard(state: tauri::State<'_, DesktopState>) -> DesktopStatus {
    state.ensure_started()
}

#[tauri::command]
fn open_dashboard_in_browser(
    state: tauri::State<'_, DesktopState>,
) -> Result<DesktopStatus, String> {
    let status = state.ensure_started();

    if let Some(url) = &status.dashboard_url {
        open_url(url).map_err(|error| format!("failed to open dashboard URL: {error}"))?;
    }

    Ok(status)
}

/// Open the Envelope that is already serving, when this app's own server
/// cannot start. Without this the failure screen is a dead end even though a
/// working dashboard is running on the machine.
#[tauri::command]
fn open_existing_dashboard(
    state: tauri::State<'_, DesktopState>,
) -> Result<DesktopStatus, String> {
    let status = state.ensure_started();

    match &status.existing_server_url {
        Some(url) => {
            open_url(url).map_err(|error| format!("failed to open {url}: {error}"))?;
            Ok(status)
        }
        None => Err("no other Envelope server is running to open".to_string()),
    }
}

#[tauri::command]
fn copy_diagnostics(state: tauri::State<'_, DesktopState>) -> CopyDiagnosticsResult {
    copy_diagnostics_for_status(state.ensure_started())
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(DesktopState::new(manifest_dir))
        .on_window_event(|window, event| {
            // Closing the window leaves Envelope running in the tray, so mail
            // keeps syncing and sending. Quit is in the tray and app menus.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
            // The window is built here rather than by the config (which now sets
            // `"create": false`) purely so a new-window handler can be attached
            // to it. Without one, WKWebView asks the app to make an auxiliary
            // webview, gets no answer, and drops the request — which is why
            // every `target="_blank"` link in an email did nothing at all.
            //
            // `from_config` and not `WebviewWindowBuilder::new`: the config
            // window has no explicit label, so it defaults to "main", and
            // capabilities/default.json grants `core:default` to "main" only.
            // A hand-written label would silently revoke every #[tauri::command]
            // this shell depends on.
            let Some(window_config) = app.config().app.windows.first().cloned() else {
                // Nothing to attach a handler to, and nothing to show. Failing
                // the setup is better than a running process with no window.
                return Err("tauri.conf.json defines no window for the desktop shell".into());
            };
            tauri::WebviewWindowBuilder::from_config(app.handle(), &window_config)?
                .on_new_window(|url, _features| {
                    open_external_url(url.as_str());
                    // Never Allow or Create. Both build a real webview from the
                    // OPENER's configuration, which on macOS carries the same
                    // WKUserContentController — so Tauri's bootstrap would
                    // inject __TAURI_INTERNALS__ and the invoke key into a page
                    // chosen by whoever sent the mail, inside a chrome-less
                    // window with no address bar to notice it in.
                    tauri::webview::NewWindowResponse::Deny
                })
                .build()?;

            install_menu(app)?;
            tray::install(app)?;
            app.on_menu_event(|app, event| {
                handle_menu_event(app, event.id().as_ref());
            });
            let state = app.state::<DesktopState>();
            let _ = state.ensure_started();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            desktop_status,
            open_dashboard,
            restart_server,
            open_dashboard_in_browser,
            open_existing_dashboard,
            copy_diagnostics,
            quit_app
        ])
        .build(tauri::generate_context!())
        .expect("error while building Envelope desktop shell")
        .run(|app, event| match event {
            // Signals (SIGTERM/SIGKILL) still bypass this; nothing in-process
            // can catch SIGKILL. This covers every ordinary quit path.
            RunEvent::Exit => app.state::<DesktopState>().shutdown(),
            // Clicking the Dock icon brings back a window closed to the tray.
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => tray::show_window(app),
            _ => {}
        });
}

/// Both output tails, so a message on either stream is classified.
fn combined_output(stdout: &Arc<Mutex<TailBuffer>>, stderr: &Arc<Mutex<TailBuffer>>) -> String {
    format!("{}\n{}", tail_to_string(stdout), tail_to_string(stderr))
}

/// A healthy Envelope already serving on the usual port.
///
/// When this app's own server cannot start — most often because an installed,
/// newer Envelope has already migrated the database past what this build
/// understands — that other server is a working way in, and the failure screen
/// should say so rather than leaving a dead end.
fn existing_dashboard_url() -> Option<String> {
    // Every probe must pass, not just "/". A server with auth configured still
    // serves the SPA shell on / while returning 401 for the API, so a
    // status-only check would offer a dashboard that loads and then cannot
    // fetch a single message — a worse dead end than saying nothing, because it
    // looks like it worked.
    let probes = check_all_health(DEFAULT_DASHBOARD_PORT, STATUS_HEALTH_TIMEOUT);
    all_health_ok(&probes).then(|| dashboard_url(DEFAULT_DASHBOARD_PORT))
}

fn wait_for_child_health(
    child: &mut Child,
    port: u16,
) -> (Vec<HealthProbe>, Option<String>, Option<String>) {
    let started = Instant::now();

    loop {
        let health = check_all_health(port, STATUS_HEALTH_TIMEOUT);
        let last_health_check_at = Some(utc_timestamp_now());

        if all_health_ok(&health) {
            return (health, None, last_health_check_at);
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                return (
                    health,
                    Some(format!(
                        "Envelope server exited during startup with {status}"
                    )),
                    last_health_check_at,
                );
            }
            Ok(None) => {}
            Err(error) => {
                return (
                    health,
                    Some(format!(
                        "failed to inspect Envelope server during startup: {error}"
                    )),
                    last_health_check_at,
                );
            }
        }

        if started.elapsed() >= STARTUP_TIMEOUT {
            return (
                health,
                Some("Envelope server did not pass all health checks before timeout".into()),
                last_health_check_at,
            );
        }

        thread::sleep(STARTUP_POLL_INTERVAL);
    }
}

fn spawn_tail_reader<R>(mut reader: R, tail: Arc<Mutex<TailBuffer>>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0; 4096];

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if let Ok(mut tail) = tail.lock() {
                        tail.push(&buffer[..read]);
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn stopped_status() -> DesktopStatus {
    DesktopStatus {
        readiness: Readiness::ServerStopped.as_str().to_string(),
        running: false,
        healthy: false,
        dashboard_url: None,
        binary_path: None,
        binary_source: None,
        binary_attempts: Vec::new(),
        port: None,
        port_source: None,
        last_health_check_at: None,
        health_checks: Vec::new(),
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        error: None,
        recommended_command: None,
        guidance: None,
        existing_server_url: None,
    }
}

fn status_from_binary_error(error: BinaryResolutionError) -> DesktopStatus {
    DesktopStatus {
        readiness: Readiness::ServerNotStarted.as_str().to_string(),
        running: false,
        healthy: false,
        dashboard_url: None,
        binary_path: error
            .attempted
            .first()
            .map(|candidate| candidate.path.display().to_string()),
        binary_source: error
            .attempted
            .first()
            .map(|candidate| candidate.source.as_str().to_string()),
        binary_attempts: binary_attempt_dtos(&error.attempted),
        port: None,
        port_source: None,
        last_health_check_at: None,
        health_checks: Vec::new(),
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        error: Some(error.to_string()),
        recommended_command: None,
        guidance: None,
        existing_server_url: existing_dashboard_url(),
    }
}

fn status_from_port_error(
    error: PortSelectionError,
    binary_path: Option<PathBuf>,
    binary_source: Option<String>,
) -> DesktopStatus {
    let binary_attempts = binary_path
        .as_ref()
        .zip(binary_source.as_ref())
        .map(|(path, source)| {
            vec![BinaryAttemptDto {
                source: source.clone(),
                path: path.display().to_string(),
            }]
        })
        .unwrap_or_default();

    DesktopStatus {
        readiness: Readiness::ServerNotStarted.as_str().to_string(),
        running: false,
        healthy: false,
        dashboard_url: None,
        binary_path: binary_path.as_ref().map(|path| path.display().to_string()),
        binary_source,
        binary_attempts,
        port: None,
        port_source: None,
        last_health_check_at: None,
        health_checks: Vec::new(),
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        error: Some(error.to_string()),
        recommended_command: None,
        guidance: None,
        existing_server_url: existing_dashboard_url(),
    }
}

fn status_from_spawn_error(
    error: io::Error,
    binary_path: PathBuf,
    binary_source: String,
    port: u16,
    port_source: String,
) -> DesktopStatus {
    DesktopStatus {
        readiness: Readiness::ServerStartFailed.as_str().to_string(),
        running: false,
        healthy: false,
        dashboard_url: Some(dashboard_url(port)),
        binary_path: Some(binary_path.display().to_string()),
        binary_source: Some(binary_source.clone()),
        binary_attempts: vec![BinaryAttemptDto {
            source: binary_source,
            path: binary_path.display().to_string(),
        }],
        port: Some(port),
        port_source: Some(port_source),
        last_health_check_at: None,
        health_checks: Vec::new(),
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        error: Some(format!("failed to start Envelope server: {error}")),
        recommended_command: Some(command_preview(&binary_path, port)),
        guidance: classify_startup_failure(&error.to_string()),
        existing_server_url: existing_dashboard_url(),
    }
}

fn health_dtos(probes: &[HealthProbe]) -> Vec<HealthCheckDto> {
    probes
        .iter()
        .map(|probe| HealthCheckDto {
            path: probe.path.clone(),
            url: probe.url.clone(),
            ok: probe.ok,
            status_code: probe.status_code,
            error: probe.error.clone(),
        })
        .collect()
}

fn binary_attempt_dtos(attempts: &[BinaryCandidate]) -> Vec<BinaryAttemptDto> {
    attempts
        .iter()
        .map(|attempt| BinaryAttemptDto {
            source: attempt.source.as_str().to_string(),
            path: attempt.path.display().to_string(),
        })
        .collect()
}

fn tail_to_string(tail: &Arc<Mutex<TailBuffer>>) -> String {
    tail.lock()
        .map(|tail| tail.as_string())
        .unwrap_or_else(|_| "(tail buffer unavailable)".to_string())
}

fn install_menu<R: tauri::Runtime>(app: &tauri::App<R>) -> tauri::Result<()> {
    let open_dashboard = MenuItem::with_id(
        app,
        MENU_OPEN_DASHBOARD,
        "Open Dashboard",
        true,
        None::<&str>,
    )?;
    let open_browser = MenuItem::with_id(
        app,
        MENU_OPEN_BROWSER,
        "Open in Browser",
        true,
        None::<&str>,
    )?;
    let copy_diagnostics = MenuItem::with_id(
        app,
        MENU_COPY_DIAGNOSTICS,
        "Copy Diagnostics",
        true,
        None::<&str>,
    )?;
    let restart_server = MenuItem::with_id(
        app,
        MENU_RESTART_SERVER,
        "Restart Envelope Server",
        true,
        None::<&str>,
    )?;
    let pause_agents = MenuItem::with_id(
        app,
        "pause_agents_unavailable",
        "Pause Agents (coming soon)",
        false,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "Quit", true, None::<&str>)?;
    let controls = Submenu::with_items(
        app,
        "Envelope",
        true,
        &[
            &open_dashboard,
            &open_browser,
            &copy_diagnostics,
            &restart_server,
            &pause_agents,
            &quit,
        ],
    )?;
    let menu = Menu::with_items(app, &[&controls])?;

    app.set_menu(menu)?;
    Ok(())
}

fn handle_menu_event<R: tauri::Runtime>(app: &tauri::AppHandle<R>, id: &str) {
    match id {
        MENU_OPEN_DASHBOARD => {
            let state = app.state::<DesktopState>();
            let status = state.ensure_started();
            let _ = app.emit(EVENT_STATUS, status.clone());

            if status.healthy {
                let _ = app.emit(EVENT_OPEN_DASHBOARD, status);
            }
        }
        MENU_OPEN_BROWSER => {
            let state = app.state::<DesktopState>();
            let status = state.ensure_started();

            if let Some(url) = &status.dashboard_url {
                let _ = open_url(url);
            }

            let _ = app.emit(EVENT_STATUS, status);
        }
        MENU_COPY_DIAGNOSTICS => {
            let state = app.state::<DesktopState>();
            let result = copy_diagnostics_for_status(state.ensure_started());
            let _ = app.emit(EVENT_COPY_RESULT, result);
        }
        MENU_RESTART_SERVER => {
            let state = app.state::<DesktopState>();
            let status = state.restart();
            let _ = app.emit(EVENT_STATUS, status);
        }
        MENU_QUIT => app.exit(0),
        _ => {}
    }
}

fn copy_diagnostics_for_status(status: DesktopStatus) -> CopyDiagnosticsResult {
    let diagnostics = diagnostics_json(&status);

    match copy_to_clipboard(&diagnostics) {
        Ok(()) => CopyDiagnosticsResult {
            status,
            diagnostics,
            copied: true,
            error: None,
        },
        Err(error) => CopyDiagnosticsResult {
            status,
            diagnostics,
            copied: false,
            error: Some(error.to_string()),
        },
    }
}

fn diagnostics_json(status: &DesktopStatus) -> String {
    let home_dir = std::env::var_os("HOME").map(PathBuf::from);
    let redacted_dashboard_url = status
        .dashboard_url
        .as_deref()
        .map(|url| redact_diagnostics_text(url, home_dir.as_deref()));
    let redacted_binary_path = status
        .binary_path
        .as_deref()
        .map(|path| redact_diagnostics_text(path, home_dir.as_deref()));
    let redacted_command = status
        .recommended_command
        .as_deref()
        .map(|command| redact_diagnostics_text(command, home_dir.as_deref()));
    let redacted_error = status
        .error
        .as_deref()
        .map(|error| redact_diagnostics_text(error, home_dir.as_deref()));
    let redacted_stdout_tail = redact_diagnostics_text(&status.stdout_tail, home_dir.as_deref());
    let redacted_stderr_tail = redact_diagnostics_text(&status.stderr_tail, home_dir.as_deref());
    let binary_attempts = status
        .binary_attempts
        .iter()
        .map(|attempt| {
            format!(
                "{{\"source\":{},\"path\":{}}}",
                json_string(&attempt.source),
                json_string(&redact_diagnostics_text(&attempt.path, home_dir.as_deref()))
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let health_checks = status
        .health_checks
        .iter()
        .map(|check| {
            format!(
                "{{\"path\":{},\"url\":{},\"ok\":{},\"statusCode\":{},\"error\":{}}}",
                json_string(&check.path),
                json_string(&redact_diagnostics_text(&check.url, home_dir.as_deref())),
                check.ok,
                option_u16_json(check.status_code),
                option_string_json(
                    check
                        .error
                        .as_deref()
                        .map(|error| redact_diagnostics_text(error, home_dir.as_deref()))
                        .as_deref()
                )
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    format!(
        concat!(
            "{{\n",
            "  \"app\": \"Envelope Desktop\",\n",
            "  \"timestamp\": {},\n",
            "  \"readiness\": {},\n",
            "  \"running\": {},\n",
            "  \"healthy\": {},\n",
            "  \"dashboardUrl\": {},\n",
            "  \"binaryPath\": {},\n",
            "  \"binarySource\": {},\n",
            "  \"binaryAttempts\": [{}],\n",
            "  \"port\": {},\n",
            "  \"portSource\": {},\n",
            "  \"lastHealthCheckAt\": {},\n",
            "  \"recommendedCommand\": {},\n",
            "  \"healthChecks\": [{}],\n",
            "  \"stdoutTail\": {},\n",
            "  \"stderrTail\": {},\n",
            "  \"error\": {}\n",
            "}}"
        ),
        json_string(&utc_timestamp_now()),
        json_string(&status.readiness),
        status.running,
        status.healthy,
        option_string_json(redacted_dashboard_url.as_deref()),
        option_string_json(redacted_binary_path.as_deref()),
        option_string_json(status.binary_source.as_deref()),
        binary_attempts,
        option_u16_json(status.port),
        option_string_json(status.port_source.as_deref()),
        option_string_json(status.last_health_check_at.as_deref()),
        option_string_json(redacted_command.as_deref()),
        health_checks,
        json_string(&redacted_stdout_tail),
        json_string(&redacted_stderr_tail),
        option_string_json(redacted_error.as_deref()),
    )
}

fn option_u16_json(value: Option<u16>) -> String {
    value
        .map(|number| number.to_string())
        .unwrap_or_else(|| "null".to_string())
}

fn option_string_json(value: Option<&str>) -> String {
    value.map(json_string).unwrap_or_else(|| "null".to_string())
}

fn json_string(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() + 2);
    encoded.push('"');

    for char in value.chars() {
        match char {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            c if c < ' ' => {
                encoded.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => encoded.push(c),
        }
    }

    encoded.push('"');
    encoded
}

fn copy_to_clipboard(text: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        return write_to_command_stdin("pbcopy", &[], text);
    }

    #[cfg(target_os = "windows")]
    {
        return write_to_command_stdin("clip", &[], text);
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        write_to_command_stdin("wl-copy", &[], text)
            .or_else(|_| write_to_command_stdin("xclip", &["-selection", "clipboard"], text))
            .or_else(|_| write_to_command_stdin("xsel", &["--clipboard", "--input"], text))
    }
}

fn write_to_command_stdin(program: &str, args: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }

    let status = child.wait()?;

    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{program} exited with status {status}"
        )))
    }
}

fn utc_timestamp_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);

    format_unix_timestamp(seconds)
}

fn format_unix_timestamp(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };

    (year, month, day)
}

/// Hand a URL that came out of an email to the operating system — but only if
/// it is one of the four schemes a mail link may legitimately use.
///
/// `open_url` below hands its argument to LaunchServices, which will happily
/// act on `file:`, `smb:`, `vnc:`, `ftp:`, and any custom scheme some installed
/// app has registered. That was safe only for as long as every caller passed a
/// URL this shell had built itself. A link in a message is chosen by the sender,
/// so it gets an allowlist.
///
/// The sanitizer already drops non-allowlisted schemes before render; this is
/// the second, independent check, on the side of the boundary that can actually
/// launch something.
fn open_external_url(url: &str) {
    if !is_openable_external_url(url) {
        eprintln!("refused to open a message link: {url}");
        return;
    }
    if let Err(e) = open_url(url) {
        eprintln!("failed to open external URL: {e}");
    }
}

/// Whether a URL from a message may be handed to the OS.
///
/// Split from `open_external_url` so the policy is testable without launching
/// anything, and fails closed: an unparseable URL is refused, never passed
/// through on the theory that the OS will sort it out.
fn is_openable_external_url(url: &str) -> bool {
    tauri::Url::parse(url)
        .map(|u| matches!(u.scheme(), "http" | "https" | "mailto" | "tel"))
        .unwrap_or(false)
}

fn open_url(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // `--` so a URL starting with a dash cannot be read as an option.
        Command::new("open").arg("--").arg(url).spawn()?.wait()?;
        return Ok(());
    }

    #[cfg(not(target_os = "macos"))]
    {
        Command::new("xdg-open").arg("--").arg(url).spawn()?.wait()?;
        Ok(())
    }
}

#[cfg(test)]
mod link_policy_tests {
    use super::is_openable_external_url;

    #[test]
    fn ordinary_message_links_are_openable() {
        for url in [
            "https://example.com/offer",
            "http://legacy.example.org/page",
            "mailto:someone@example.com?subject=Re:%20hello",
            "tel:+34600123456",
            "HTTPS://Example.COM/Mixed-Case",
        ] {
            assert!(is_openable_external_url(url), "should open: {url}");
        }
    }

    #[test]
    fn schemes_that_could_launch_something_are_refused() {
        // Every one of these reaches LaunchServices if handed to `open`, and
        // every one of them is a URL a sender can put in an email.
        for url in [
            "file:///Applications/Calculator.app",
            "file:///etc/passwd",
            "smb://attacker.example.com/share",
            "vnc://attacker.example.com",
            "ftp://attacker.example.com/x",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "about:blank",
            "tauri://localhost",
            "x-apple-helpbook://com.apple.machelp",
            "itms-apps://apps.apple.com/app/id1",
        ] {
            assert!(!is_openable_external_url(url), "must refuse: {url}");
        }
    }

    #[test]
    fn unparseable_input_fails_closed() {
        // Relative or malformed input must not be forwarded on the theory that
        // the OS will work out what was meant.
        for url in ["", "   ", "not a url", "//evil.example.com", "/etc/passwd"] {
            assert!(!is_openable_external_url(url), "must refuse: {url:?}");
        }
    }
}
