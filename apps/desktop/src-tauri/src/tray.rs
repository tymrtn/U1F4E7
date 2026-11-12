//! The menu-bar (macOS) or system-tray (Linux, Windows) icon.
//!
//! Envelope keeps running with its window closed, so the server it supervises
//! has to be visible and controllable from somewhere. The dashboard on this
//! machine was once down for five hours and nothing said so; the tray shows the
//! server's state, starts, stops and restarts it, and posts a notification when
//! a working server stops or new mail arrives.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::thread;
use std::time::Duration;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Wry};
use tauri_plugin_notification::NotificationExt;

use crate::alerts::{
    HealthAlert, HealthWatch, SseParser, UnreadTracker, new_mail_text, parse_account_names,
    parse_new_mail, status_line,
};
use crate::supervisor::{Readiness, parse_status_code};
use crate::{DesktopState, DesktopStatus, EVENT_STATUS};

const TRAY_ID: &str = "envelope";
const TRAY_STATUS: &str = "tray_status";
const TRAY_OPEN: &str = "tray_open";
const TRAY_TOGGLE: &str = "tray_toggle_server";
const TRAY_RESTART: &str = "tray_restart_server";
const TRAY_QUIT: &str = "tray_quit";

const WATCH_INTERVAL: Duration = Duration::from_secs(5);
/// The server sends a heartbeat every 25s, so a minute of silence means the
/// connection is dead even if no error surfaced.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(60);
const STREAM_RETRY: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

struct TrayItems {
    status: MenuItem<Wry>,
    toggle: MenuItem<Wry>,
}

pub fn install(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let status = MenuItem::with_id(app, TRAY_STATUS, "Starting…", false, None::<&str>)?;
    let open = MenuItem::with_id(app, TRAY_OPEN, "Open Envelope", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, TRAY_TOGGLE, "Stop Server", true, None::<&str>)?;
    let restart = MenuItem::with_id(app, TRAY_RESTART, "Restart Server", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, TRAY_QUIT, "Quit Envelope", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &open,
            &PredefinedMenuItem::separator(app)?,
            &toggle,
            &restart,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    // Everything goes through the menu: Linux tray hosts do not report clicks
    // on the icon itself, only menu selections.
    tray_icon(app)?
        .tooltip("Envelope")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| handle_menu_event(app, event.id().as_ref()))
        .build(app)?;

    app.manage(TrayItems { status, toggle });

    let handle = app.handle().clone();
    thread::spawn(move || watch_server(handle));
    let handle = app.handle().clone();
    thread::spawn(move || watch_mail(handle));

    Ok(())
}

/// The macOS menu bar wants a monochrome template it can tint for light and
/// dark mode; the full-colour app tile reads as a dark blot there.
#[cfg(target_os = "macos")]
fn tray_icon(app: &tauri::App) -> tauri::Result<TrayIconBuilder<Wry>> {
    let _ = app;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray-template.png"))?;
    Ok(TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .icon_as_template(true))
}

/// Linux and Windows panels vary in colour and do not tint, so they get the
/// app icon.
#[cfg(not(target_os = "macos"))]
fn tray_icon(app: &tauri::App) -> Result<TrayIconBuilder<Wry>, Box<dyn std::error::Error>> {
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or("the bundle defines no icon for the tray")?;
    Ok(TrayIconBuilder::with_id(TRAY_ID).icon(icon))
}

pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn handle_menu_event(app: &AppHandle, id: &str) {
    match id {
        TRAY_OPEN => show_window(app),
        TRAY_TOGGLE => {
            if app.state::<DesktopState>().server_running() {
                let status = app.state::<DesktopState>().stop();
                publish(app, &status);
            } else {
                start_in_background(app);
            }
        }
        TRAY_RESTART => start_in_background(app),
        TRAY_QUIT => app.exit(0),
        _ => {}
    }
}

/// Starting waits up to fifteen seconds for health checks. Menu events arrive
/// on the main thread, so doing it there would freeze the window meanwhile.
fn start_in_background(app: &AppHandle) {
    set_tray_text(app, "Starting…", None);
    let app = app.clone();
    thread::spawn(move || {
        let status = app.state::<DesktopState>().restart();
        publish(&app, &status);
    });
}

/// Show a status the user caused in both the tray and the window.
fn publish(app: &AppHandle, status: &DesktopStatus) {
    update_tray(app, status);
    let _ = app.emit(EVENT_STATUS, status.clone());
}

fn update_tray(app: &AppHandle, status: &DesktopStatus) {
    let toggle = if status.running {
        "Stop Server"
    } else {
        "Start Server"
    };
    set_tray_text(app, status_line(&status.readiness), Some(toggle));
}

fn set_tray_text(app: &AppHandle, line: &str, toggle: Option<&str>) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.status.set_text(line);
        if let Some(toggle) = toggle {
            let _ = items.toggle.set_text(toggle);
        }
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(format!("Envelope: {line}")));
    }
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    if let Err(error) = app.notification().builder().title(title).body(body).show() {
        eprintln!("could not show notification \"{title}\": {error}");
    }
}

fn watch_server(app: AppHandle) {
    let mut health = HealthWatch::default();
    loop {
        thread::sleep(WATCH_INTERVAL);
        let Some(status) = app.state::<DesktopState>().peek() else {
            continue;
        };
        update_tray(&app, &status);

        let stopped_by_user = status.readiness == Readiness::ServerStopped.as_str();
        match health.observe(status.healthy, stopped_by_user) {
            Some(HealthAlert::Down) => notify(
                &app,
                "Envelope server stopped",
                "Mail isn't syncing or sending. Restart it from the Envelope menu.",
            ),
            Some(HealthAlert::Recovered) => notify(&app, "Envelope is running again", ""),
            None => {}
        }
    }
}

fn watch_mail(app: AppHandle) {
    let mut unread = UnreadTracker::default();
    loop {
        let port = app
            .state::<DesktopState>()
            .peek()
            .filter(|status| status.healthy)
            .and_then(|status| status.port);

        if let Some(port) = port {
            let mut names = account_names(port);
            let result = follow_events(port, |event, data| {
                if event != "new_mail" {
                    return;
                }
                let Some((account_id, count)) = parse_new_mail(data) else {
                    eprintln!("mail alerts: unreadable new_mail event: {data}");
                    return;
                };
                let Some(grew) = unread.observe(&account_id, count) else {
                    return;
                };
                if !names.contains_key(&account_id) {
                    names = account_names(port);
                }
                let text = new_mail_text(grew, names.get(&account_id).map(String::as_str));
                notify(&app, "Envelope", &text);
            });
            if let Err(error) = result {
                eprintln!("mail alerts: event stream on port {port} ended: {error}");
            }
        }

        thread::sleep(STREAM_RETRY);
    }
}

fn connect(port: u16) -> io::Result<TcpStream> {
    let socket = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&socket, CONNECT_TIMEOUT)
}

/// Send a GET and return the reader positioned at the start of the body.
///
/// HTTP/1.0 on purpose: the server then cannot answer a stream with chunked
/// transfer encoding, so the body is the raw bytes until the connection closes
/// and no chunk decoder is needed.
fn get(port: u16, path: &str, accept: &str, timeout: Duration) -> io::Result<BufReader<TcpStream>> {
    let mut stream = connect(port)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAccept: {accept}\r\n\r\n"
    )?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if parse_status_code(&line) != Some(200) {
        return Err(io::Error::other(format!(
            "GET {path} answered {:?}",
            line.trim_end()
        )));
    }
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("GET {path} closed before its body"),
            ));
        }
        if line.trim_end().is_empty() {
            return Ok(reader);
        }
    }
}

fn account_names(port: u16) -> HashMap<String, String> {
    let body =
        get(port, "/api/accounts", "application/json", CONNECT_TIMEOUT).and_then(|mut reader| {
            let mut body = String::new();
            reader.read_to_string(&mut body)?;
            Ok(body)
        });
    match body {
        Ok(body) => parse_account_names(&body),
        Err(error) => {
            eprintln!("mail alerts: could not list accounts: {error}");
            HashMap::new()
        }
    }
}

/// Follow the server's event stream until it closes or goes silent.
fn follow_events(port: u16, mut on_event: impl FnMut(&str, &str)) -> io::Result<()> {
    let mut reader = get(
        port,
        "/api/events/stream",
        "text/event-stream",
        STREAM_READ_TIMEOUT,
    )?;
    let mut parser = SseParser::default();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if let Some((event, data)) = parser.push_line(line.trim_end_matches('\n')) {
            on_event(&event, &data);
        }
    }
}
