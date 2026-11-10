//! One server per mail home runs the background jobs: sync, add-ons, shelf
//! life, digests, scheduled sends and stale-send recovery. A second `serve` on
//! the same home (the desktop app beside the installed dashboard) serves its
//! window and leaves that work to the first, and picks it up when the first
//! stops. Two servers running it at once each recover the other's in-flight
//! sends at startup and race each other on every arrival.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use envelope_email_store::Database;
use envelope_email_store::models::IndexedMessageInput;

const RUNS_BACKGROUND: &str = "Background Inbox index refresh running";
const DEFERS: &str = "Another Envelope server on this mail home runs the background jobs";

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn forward(stream: impl std::io::Read + Send + 'static, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

/// A running `envelope serve`, killed on drop so a failed assertion leaves no
/// server behind.
struct Serve {
    child: Child,
    port: u16,
    lines: Receiver<String>,
    seen: String,
}

impl Serve {
    fn start(home: &Path) -> Self {
        let port = free_port();
        let mut child = Command::new(envelope_bin())
            .args(["serve", "--port", &port.to_string()])
            .env("HOME", home)
            .env("ENVELOPE_HOME", home)
            .env_remove("RUST_LOG")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn serve");
        let (tx, lines) = mpsc::channel();
        // stderr rides along so a failure shows why a server exited.
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        forward(stdout, tx.clone());
        forward(stderr, tx);
        Self {
            child,
            port,
            lines,
            seen: String::new(),
        }
    }

    /// Read stdout until a line contains `needle` or `limit` passes.
    fn shows(&mut self, needle: &str, limit: Duration) -> bool {
        if self.seen.contains(needle) {
            return true;
        }
        let deadline = Instant::now() + limit;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push_str(&line);
                    self.seen.push('\n');
                    if line.contains(needle) {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
        false
    }

    fn stop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn a_second_server_on_one_home_leaves_background_work_to_the_first_until_it_stops() {
    let home = tempfile::tempdir().expect("temp home");

    let mut first = Serve::start(home.path());
    assert!(
        first.shows(RUNS_BACKGROUND, Duration::from_secs(20)),
        "the first server must run the background jobs:\n{}",
        first.seen
    );

    let mut second = Serve::start(home.path());
    assert!(
        second.shows(DEFERS, Duration::from_secs(20)),
        "a second server on the same home must say it leaves the background jobs to the first:\n{}",
        second.seen
    );
    assert!(
        !second.shows(RUNS_BACKGROUND, Duration::from_secs(3)),
        "a second server on the same home must not run the background jobs too:\n{}",
        second.seen
    );

    first.stop();
    assert!(
        second.shows(RUNS_BACKGROUND, Duration::from_secs(20)),
        "once the first server stops, the second must take the background jobs over:\n{}",
        second.seen
    );
}

#[test]
fn servers_on_different_homes_each_run_their_own_background_work() {
    let one = tempfile::tempdir().expect("temp home");
    let two = tempfile::tempdir().expect("temp home");

    let mut first = Serve::start(one.path());
    let mut second = Serve::start(two.path());
    assert!(
        first.shows(RUNS_BACKGROUND, Duration::from_secs(20)),
        "{}",
        first.seen
    );
    assert!(
        second.shows(RUNS_BACKGROUND, Duration::from_secs(20)),
        "a server on another home must not wait on this one:\n{}",
        second.seen
    );
}

/// One account with one read message indexed in INBOX, written straight to
/// the home's database the way a sync would.
fn index_inbox(home: &Path, unread_uids: &[u32]) {
    let db = Database::open(&home.join("envelope-email").join("envelope.db")).expect("open db");
    db.conn()
        .execute(
            "INSERT OR IGNORE INTO accounts (id, name, username, domain, smtp_host, smtp_port, imap_host, imap_port, encrypted_password)
             VALUES ('acct-a', 'Test', 'a@example.invalid', 'example.invalid', 'smtp.example.invalid', 587, 'imap.example.invalid', 993, 'x')",
            [],
        )
        .expect("insert account");
    let row = |uid: u32, seen: bool| IndexedMessageInput {
        uid,
        message_id: Some(format!("<m{uid}@example.invalid>")),
        from_addr: "sender@example.invalid".into(),
        to_addr: "a@example.invalid".into(),
        subject: format!("message {uid}"),
        date: Some("Mon, 24 Aug 2026 10:00:00 +0000".into()),
        flags: if seen { vec!["\\Seen".into()] } else { vec![] },
        size: 1,
        snippet: None,
        thread_id: None,
    };
    let mut rows = vec![row(1, true)];
    rows.extend(unread_uids.iter().map(|uid| row(*uid, false)));
    db.upsert_indexed_message_summaries("acct-a", "INBOX", 1, &rows)
        .expect("index");
}

/// Follow `port`'s event stream until an event named `name` arrives or
/// `limit` passes.
fn stream_shows(port: u16, name: &str, limit: Duration, act: impl FnOnce()) -> (bool, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("timeout");
    write!(
        stream,
        "GET /api/events/stream HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\n\r\n"
    )
    .expect("request");
    let mut reader = BufReader::new(stream);
    let mut seen = String::new();
    let mut act = Some(act);
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                seen.push_str(&line);
                if line.trim_end() == format!("event: {name}") {
                    return (true, seen);
                }
            }
            Err(_) => {}
        }
        // Subscribed once the response headers are in: act then.
        if seen.contains("\r\n\r\n") || seen.lines().any(|l| l.trim().is_empty()) {
            if let Some(act) = act.take() {
                act();
            }
        }
    }
    (false, seen)
}

#[test]
fn a_deferring_server_still_announces_mail_the_owners_sync_indexed() {
    let home = tempfile::tempdir().expect("temp home");

    let mut owner = Serve::start(home.path());
    assert!(
        owner.shows(RUNS_BACKGROUND, Duration::from_secs(20)),
        "{}",
        owner.seen
    );
    index_inbox(home.path(), &[]);

    let mut deferring = Serve::start(home.path());
    assert!(
        deferring.shows(DEFERS, Duration::from_secs(20)),
        "{}",
        deferring.seen
    );

    // The owner's sync writes a new unread message; the deferring server's
    // stream (what the desktop tray follows) must announce it.
    let (found, seen) = stream_shows(deferring.port, "new_mail", Duration::from_secs(20), || {
        index_inbox(home.path(), &[2])
    });
    assert!(
        found,
        "the deferring server must announce mail another server indexed:\n{seen}\n{}",
        deferring.seen
    );
}
