// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! What the CLI lets a caller do when `ENVELOPE_AGENT_TOKEN` is set. Every
//! test runs the built binary in an isolated `HOME`. The seeded account points
//! IMAP and SMTP at a local listener, so a command that should have been
//! refused but ran anyway shows up as a connection there, never on a network.

use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn base_command(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(envelope_bin());
    cmd.args(args)
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .env_remove("ENVELOPE_AGENT_TOKEN")
        .env_remove("ENVELOPE_MCP_UNSAFE_ALLOW_ANONYMOUS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Run to completion, killing the process if it outlives `limit` (a refused
/// `serve` exits at once; one that was not refused would run forever).
fn finish(mut cmd: Command, limit: Duration) -> (Output, bool) {
    use std::io::Read;
    let mut child = cmd.spawn().expect("spawn envelope");
    // Drain both pipes while waiting, so a large output cannot block the child.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().expect("poll envelope") {
            break (status, false);
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            break (child.wait().expect("reap envelope"), true);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = Output {
        status,
        stdout: stdout.join().expect("stdout reader"),
        stderr: stderr.join().expect("stderr reader"),
    };
    (output, timed_out)
}

fn run(home: &Path, args: &[&str]) -> Output {
    finish(base_command(home, args), Duration::from_secs(30)).0
}

fn run_as(home: &Path, token: &str, args: &[&str]) -> (Output, bool) {
    let mut cmd = base_command(home, args);
    cmd.env("ENVELOPE_AGENT_TOKEN", token);
    finish(cmd, Duration::from_secs(10))
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout was not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// A local stand-in for the mail servers that counts connections.
struct MailProbe {
    port: u16,
    connections: Arc<AtomicUsize>,
}

impl MailProbe {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let port = listener.local_addr().expect("probe address").port();
        let connections = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&connections);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                seen.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        Self { port, connections }
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

fn seed_account(home: &Path, email: &str, probe: &MailProbe) {
    let port = probe.port.to_string();
    let mut cmd = base_command(
        home,
        &[
            "accounts",
            "add",
            "--skip-login-check",
            "--email",
            email,
            "--password-stdin",
            "--smtp-host",
            "127.0.0.1",
            "--smtp-port",
            &port,
            "--imap-host",
            "127.0.0.1",
            "--imap-port",
            &port,
            "--insecure-machine-key",
            "--json",
        ],
    );
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn accounts add");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"pw\n")
        .expect("write password");
    let out = child.wait_with_output().expect("wait accounts add");
    assert!(
        out.status.success(),
        "seed account failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn create_token(home: &Path, name: &str) -> String {
    let created = stdout_json(&run(home, &["--json", "agent", "create", name]));
    created["token"].as_str().expect("agent token").to_string()
}

fn set_actions(home: &Path, name: &str, actions: &str) {
    let out = run(
        home,
        &["agent", "policy", "set", name, "--allow-actions", actions],
    );
    assert!(out.status.success(), "policy set failed");
}

fn db(home: &Path) -> envelope_email_store::Database {
    envelope_email_store::Database::open(&home.join("envelope-email/envelope.db"))
        .expect("open isolated test db")
}

fn count(home: &Path, sql: &str) -> i64 {
    db(home)
        .conn()
        .query_row(sql, [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Everything a refused command must leave untouched.
fn snapshot(home: &Path) -> Vec<i64> {
    [
        "SELECT COUNT(*) FROM agent_identities",
        "SELECT COUNT(*) FROM agent_identities WHERE revoked_at IS NOT NULL",
        "SELECT COUNT(*) FROM agent_policies WHERE send_mode_ceiling != 'draft-only'",
        "SELECT COUNT(*) FROM accounts",
        "SELECT COUNT(*) FROM rules",
        "SELECT COUNT(*) FROM rules WHERE enabled = 1",
        "SELECT COUNT(*) FROM event_routes",
        "SELECT COUNT(*) FROM drafts",
        "SELECT COUNT(*) FROM contacts",
    ]
    .iter()
    .map(|sql| count(home, sql))
    .collect()
}

fn config_bytes(home: &Path) -> Option<Vec<u8>> {
    std::fs::read(home.join("envelope-email/config.json")).ok()
}

fn assert_refused(out: &Output, timed_out: bool, code: &str, args: &[&str]) {
    assert!(!timed_out, "{args:?} kept running instead of being refused");
    assert!(
        !out.status.success(),
        "{args:?} must exit nonzero; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let payload = stdout_json(out);
    assert_eq!(payload["status"], "denied", "{args:?}: {payload}");
    assert_eq!(payload["error"]["code"], code, "{args:?}: {payload}");
    assert!(
        payload["error"]["reason"].is_string(),
        "{args:?}: {payload}"
    );
}

struct Fixture {
    _temp: tempfile::TempDir,
    probe: MailProbe,
    rule_name: &'static str,
    account: &'static str,
}

impl Fixture {
    fn home(&self) -> &Path {
        self._temp.path()
    }
}

/// One account behind the local probe, one disabled operator rule, and one
/// agent named `skippy` whose policy is the default (`*` actions,
/// draft-only ceiling).
fn fixture() -> (Fixture, String) {
    let temp = tempfile::tempdir().expect("temp HOME");
    let probe = MailProbe::start();
    seed_account(temp.path(), "me@example.test", &probe);
    let created = run(
        temp.path(),
        &[
            "rule",
            "create",
            "--name",
            "archive",
            "--match-from",
            "*@sender.example",
            "--action",
            "move=Archive",
            "--disabled",
        ],
    );
    assert!(created.status.success(), "operator rule create failed");
    let token = create_token(temp.path(), "skippy");
    (
        Fixture {
            _temp: temp,
            probe,
            rule_name: "archive",
            account: "me@example.test",
        },
        token,
    )
}

/// Commands an agent token may never run.
fn operator_only_commands(f: &Fixture) -> Vec<Vec<String>> {
    let account = f.account;
    let restore_dir = f.home().join("no-archive");
    let restore_dir = restore_dir.to_str().unwrap();
    let export_dir = f.home().join("no-export");
    let export_dir = export_dir.to_str().unwrap();
    [
        vec!["agent", "create", "minted"],
        vec!["agent", "revoke", "skippy"],
        vec![
            "agent",
            "policy",
            "set",
            "skippy",
            "--allow-actions",
            "*,rules.write,rules.webhook,watch.webhook,unsubscribe",
            "--send-mode-ceiling",
            "autonomous-send",
        ],
        vec![
            "config",
            "set",
            "dashboard.auth_token",
            "agent-chosen-token",
        ],
        vec![
            "config",
            "set",
            "threat.receiver_domain",
            "me@example.test=attacker.example",
        ],
        vec![
            "tag",
            "set",
            "1",
            "--tag",
            "threat:false_positive",
            "--account",
            account,
        ],
        vec![
            "bulk",
            "tag",
            "--tag",
            "threat:malware",
            "--uids",
            "1",
            "--account",
            account,
        ],
        vec!["config", "set", "otp.allow_unverified_senders", account],
        vec!["config", "unset", "threat.enabled"],
        vec![
            "serve",
            "--port",
            "0",
            "--bind",
            "127.0.0.1",
            "--no-auth",
            "--no-background-sweeps",
        ],
        vec![
            "accounts",
            "add",
            "--skip-login-check",
            "--email",
            "second@example.test",
            "--password-stdin",
            "--smtp-host",
            "127.0.0.1",
            "--imap-host",
            "127.0.0.1",
            "--insecure-machine-key",
        ],
        vec!["accounts", "remove", account],
        vec![
            "accounts",
            "signature",
            "set",
            "--account",
            account,
            "--text",
            "sig",
        ],
        vec![
            "events",
            "routes",
            "add",
            "--url",
            "https://example.invalid/hook",
        ],
        vec!["events", "routes", "remove", "evrt_none"],
        vec!["events", "deliveries", "retry", "dlv_none"],
        vec!["migrate", "run", "--from", account, "--to", account],
        vec![
            "backup",
            "restore",
            "--account",
            account,
            "--from",
            restore_dir,
        ],
        vec!["threat", "mark-safe", "1", "--account", account],
        vec!["threat", "release", "1", "--account", account],
        vec!["actions", "confirm", "evt_none"],
        vec!["actions", "dismiss", "evt_none"],
        vec!["contacts", "add", "--email", "friend@example.test"],
        vec!["contacts", "tag", "friend@example.test", "--tag", "vip"],
        vec![
            "attachment",
            "download",
            "1",
            "invoice.exe",
            "--unsafe",
            "--account",
            account,
        ],
        vec![
            "evidence",
            "attachment",
            "export",
            "--account",
            account,
            "--uid",
            "1",
            "--out",
            export_dir,
            "--unsafe",
        ],
        vec![
            "rule",
            "publish-sieve",
            "--account",
            account,
            "--host",
            "sieve.attacker.example",
            "--confirm",
        ],
        vec![
            "rule",
            "publish-sieve",
            "--account",
            account,
            "--host",
            "sieve.attacker.example",
        ],
        vec!["doctor", "--repair", "--account", account],
        vec!["contacts", "import", "--account", account],
        vec![
            "actions",
            "exec",
            "--event-id",
            "evt_none",
            "--actor",
            "tyler",
            "mark-handled",
        ],
    ]
    .into_iter()
    .map(|args| args.into_iter().map(str::to_string).collect())
    .collect()
}

/// Gated commands whose action a `*` policy does not grant.
fn named_grant_commands(f: &Fixture) -> Vec<Vec<String>> {
    let account = f.account;
    [
        vec![
            "rule",
            "create",
            "--name",
            "everything",
            "--match-from",
            "*",
            "--action",
            "delete",
        ],
        vec!["rule", "enable", f.rule_name],
        vec!["rule", "disable", f.rule_name],
        vec!["rule", "delete", f.rule_name],
        vec![
            "watch",
            "--account",
            account,
            "--webhook",
            "https://example.invalid/hook",
        ],
        vec![
            "unsubscribe",
            "1",
            "--account",
            account,
            "--confirm",
            "--attr",
            "informational",
        ],
        vec!["rule", "publish-sieve", "--account", account, "--confirm"],
        vec!["watch", "--account", account, "--deliver"],
    ]
    .into_iter()
    .map(|args| args.into_iter().map(str::to_string).collect())
    .collect()
}

/// Gated commands that reuse the MCP action names, which `*` does grant.
fn mailbox_commands(f: &Fixture) -> Vec<Vec<String>> {
    let account = f.account;
    [
        vec![
            "send",
            "--to",
            "a@b.test",
            "--subject",
            "hi",
            "--body",
            "x",
            "--attr",
            "informational",
        ],
        vec!["move", "1", "--to-folder", "Archive", "--account", account],
        vec!["copy", "1", "--to-folder", "Archive", "--account", account],
        vec![
            "delete",
            "1",
            "--permanent",
            "--confirm",
            "--account",
            account,
        ],
        vec!["flag", "add", "1", "seen", "--account", account],
        vec![
            "bulk",
            "delete",
            "--uids",
            "1",
            "--confirm",
            "--account",
            account,
        ],
        vec!["tag", "set", "1", "--tag", "x", "--account", account],
        vec![
            "snooze",
            "set",
            "1",
            "--until",
            "tomorrow",
            "--account",
            account,
        ],
        vec!["unsnooze", "--once", "--account", account],
        vec!["draft", "create", "--to", "a@b.test", "--account", account],
        vec!["draft", "discard", "missing-draft"],
        vec!["draft", "send", "missing-draft", "--attr", "informational"],
        vec!["scheduled", "hold", "missing-draft"],
        vec!["threat", "report", "1", "--account", account],
        vec!["rule", "run", "--confirm", "--account", account],
        vec!["watch", "--account", account, "--run-rules"],
    ]
    .into_iter()
    .map(|args| args.into_iter().map(str::to_string).collect())
    .collect()
}

fn with_json(args: &[String]) -> Vec<&str> {
    std::iter::once("--json")
        .chain(args.iter().map(String::as_str))
        .collect()
}

#[test]
fn agent_token_never_runs_operator_commands() {
    let (f, token) = fixture();
    let home = f.home();
    let before = snapshot(home);
    let config_before = config_bytes(home);

    for args in operator_only_commands(&f) {
        let args = with_json(&args);
        let (out, timed_out) = run_as(home, &token, &args);
        assert_refused(&out, timed_out, "operator_only_command", &args);
    }

    assert_eq!(snapshot(home), before, "a refused command changed state");
    assert_eq!(config_bytes(home), config_before, "config.json changed");
    assert_eq!(
        f.probe.connections(),
        0,
        "a refused command reached the mail servers"
    );
}

#[test]
fn star_policy_does_not_grant_named_actions() {
    let (f, token) = fixture();
    let home = f.home();
    let before = snapshot(home);

    for args in named_grant_commands(&f) {
        let args = with_json(&args);
        let (out, timed_out) = run_as(home, &token, &args);
        assert_refused(&out, timed_out, "agent_policy_denied_action", &args);
    }

    assert_eq!(snapshot(home), before, "a refused command changed state");
    assert_eq!(
        f.probe.connections(),
        0,
        "a refused command reached the mail servers"
    );
}

#[test]
fn mailbox_writes_need_their_policy_action() {
    let (f, token) = fixture();
    let home = f.home();
    set_actions(home, "skippy", "inbox.read");
    let before = snapshot(home);

    for args in mailbox_commands(&f) {
        let args = with_json(&args);
        let (out, timed_out) = run_as(home, &token, &args);
        assert_refused(&out, timed_out, "agent_policy_denied_action", &args);
    }

    assert_eq!(snapshot(home), before, "a refused command changed state");
    assert_eq!(
        f.probe.connections(),
        0,
        "a refused command reached the mail servers"
    );
}

#[test]
fn match_all_delete_rule_needs_rules_write() {
    let (f, token) = fixture();
    let home = f.home();
    let rule = [
        "--json",
        "rule",
        "create",
        "--name",
        "everything",
        "--match-from",
        "*",
        "--action",
        "delete",
    ];
    set_actions(home, "skippy", "rules.read,rules.run,delete,move");
    let (out, timed_out) = run_as(home, &token, &rule);
    assert_refused(&out, timed_out, "agent_policy_denied_action", &rule);
    assert_eq!(count(home, "SELECT COUNT(*) FROM rules"), 1);

    set_actions(home, "skippy", "rules.write");
    let (out, _) = run_as(home, &token, &rule);
    assert!(
        out.status.success(),
        "rules.write allows it: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(count(home, "SELECT COUNT(*) FROM rules"), 2);
}

#[test]
fn watch_webhook_and_unsubscribe_run_with_their_grants() {
    let (f, token) = fixture();
    let home = f.home();
    set_actions(home, "skippy", "watch.webhook,unsubscribe");
    for args in [
        vec![
            "--json",
            "watch",
            "--account",
            f.account,
            "--webhook",
            "https://example.invalid/hook",
        ],
        vec![
            "--json",
            "unsubscribe",
            "1",
            "--account",
            f.account,
            "--confirm",
            "--attr",
            "informational",
        ],
    ] {
        let (out, timed_out) = run_as(home, &token, &args);
        assert!(!timed_out || args[1] == "watch", "{args:?} hung");
        if !out.stdout.is_empty()
            && let Ok(payload) = serde_json::from_slice::<Value>(&out.stdout)
        {
            assert_ne!(payload["status"], "denied", "{args:?}: {payload}");
        }
    }
    assert!(
        f.probe.connections() >= 1,
        "with the grant, the commands go on to the mail server"
    );
}

#[test]
fn only_reading_happens_in_quarantine() {
    let (f, token) = fixture();
    let home = f.home();
    let account = f.account;
    let before = snapshot(home);

    // The default `*` policy grants every one of these elsewhere.
    for args in [
        vec!["delete", "1", "--folder", "Envelope/Quarantine"],
        vec![
            "delete",
            "1",
            "--permanent",
            "--confirm",
            "--folder",
            "INBOX.Envelope.Quarantine",
        ],
        vec![
            "snooze",
            "set",
            "1",
            "--until",
            "2h",
            "--folder",
            "envelope/quarantine/",
        ],
        vec![
            "flag",
            "add",
            "1",
            "seen",
            "--folder",
            "Envelope/Quarantine",
        ],
        vec![
            "tag",
            "set",
            "1",
            "--tag",
            "x",
            "--folder",
            "Envelope/Quarantine",
        ],
        vec![
            "bulk",
            "delete",
            "--uids",
            "1",
            "--confirm",
            "--folder",
            "Envelope/Quarantine",
        ],
        vec!["draft", "forward", "1", "--folder", "Envelope/Quarantine"],
    ] {
        let mut args: Vec<&str> = std::iter::once("--json").chain(args).collect();
        args.extend_from_slice(&["--account", account]);
        let (out, timed_out) = run_as(home, &token, &args);
        assert_refused(&out, timed_out, "operator_only_command", &args);
    }
    assert_eq!(snapshot(home), before, "a refused command changed state");
    assert_eq!(
        f.probe.connections(),
        0,
        "a refused command reached the mail servers"
    );

    // Reading there is allowed, and goes on to the mail server.
    let args = [
        "--json",
        "inbox",
        "--folder",
        "Envelope/Quarantine",
        "--account",
        account,
    ];
    let (out, _) = run_as(home, &token, &args);
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!output.contains("\"denied\""), "{output}");
    assert!(
        f.probe.connections() >= 1,
        "the read reached the mail server"
    );
}

#[test]
fn star_can_add_the_named_grants() {
    let (f, token) = fixture();
    let home = f.home();

    let mixed = run(
        home,
        &[
            "agent",
            "policy",
            "set",
            "skippy",
            "--allow-actions",
            "*,inbox.read",
        ],
    );
    assert!(
        !mixed.status.success(),
        "`*` with an ordinary action is refused"
    );
    let reason = String::from_utf8_lossy(&mixed.stderr);
    for action in [
        "rules.write",
        "rules.webhook",
        "rules.batch_ack",
        "sieve.publish",
        "watch.webhook",
        "unsubscribe",
    ] {
        assert!(
            reason.contains(action),
            "the refusal names {action}: {reason}"
        );
    }

    set_actions(home, "skippy", "*,watch.webhook");
    let shown = stdout_json(&run(home, &["--json", "agent", "policy", "show", "skippy"]));
    assert_eq!(
        shown["allowed_actions"],
        serde_json::json!(["*", "watch.webhook"]),
        "{shown}"
    );

    // `*` still grants the ordinary actions, and the named one is granted too.
    for args in [
        vec![
            "--json",
            "draft",
            "create",
            "--to",
            "a@b.test",
            "--account",
            f.account,
        ],
        vec![
            "--json",
            "watch",
            "--account",
            f.account,
            "--webhook",
            "https://example.invalid/hook",
        ],
    ] {
        let (out, _) = run_as(home, &token, &args);
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !output.contains("agent_policy_denied_action"),
            "{args:?}: {output}"
        );
    }

    // Named actions left off the list stay off.
    let rule = [
        "--json",
        "rule",
        "create",
        "--name",
        "everything",
        "--match-from",
        "*",
        "--action",
        "delete",
    ];
    let (out, timed_out) = run_as(home, &token, &rule);
    assert_refused(&out, timed_out, "agent_policy_denied_action", &rule);
}

#[test]
fn read_only_commands_run_with_a_token() {
    let (f, token) = fixture();
    let home = f.home();
    for args in [
        vec!["--json", "rule", "list"],
        vec!["--json", "agent", "list"],
        vec!["--json", "agent", "policy", "show", "skippy"],
        vec!["--json", "config", "get", "dashboard.base_url"],
        vec!["--json", "rule", "publish-sieve", "--account", f.account],
        vec!["--json", "events", "routes", "list"],
        vec!["contract"],
    ] {
        let (out, timed_out) = run_as(home, &token, &args);
        assert!(!timed_out, "{args:?} hung");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(f.probe.connections(), 0);
}

#[test]
fn without_a_token_operator_commands_are_unchanged() {
    let (f, _token) = fixture();
    let home = f.home();
    for args in [
        vec![
            "--json",
            "events",
            "routes",
            "add",
            "--url",
            "https://example.invalid/hook",
        ],
        vec![
            "--json",
            "rule",
            "create",
            "--name",
            "everything",
            "--match-from",
            "*",
            "--action",
            "delete",
        ],
        vec!["--json", "agent", "create", "second"],
        vec!["--json", "rule", "enable", f.rule_name],
    ] {
        let out = run(home, &args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(count(home, "SELECT COUNT(*) FROM event_routes"), 1);
    assert_eq!(count(home, "SELECT COUNT(*) FROM rules"), 2);

    let out = run(
        home,
        &[
            "--json",
            "config",
            "set",
            "threat.receiver_domain",
            "Me@Example.test=mx.self.example",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let config: Value = serde_json::from_slice(&config_bytes(home).unwrap()).unwrap();
    assert_eq!(
        config["threat"]["receiver_domain"],
        serde_json::json!({"me@example.test": "self.example"})
    );
}

#[test]
fn unknown_revoked_or_non_utf8_tokens_fail_closed_everywhere() {
    let (f, token) = fixture();
    let home = f.home();
    assert!(run(home, &["agent", "revoke", "skippy"]).status.success());

    for args in [
        vec!["--json", "rule", "list"],
        vec!["--json", "agent", "create", "minted"],
    ] {
        let (out, timed_out) = run_as(home, &token, &args);
        assert_refused(&out, timed_out, "agent_token_invalid", &args);
        let (out, timed_out) = run_as(home, "envtok_not_a_real_token", &args);
        assert_refused(&out, timed_out, "agent_token_invalid", &args);
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bad = std::ffi::OsStr::from_bytes(b"envtok_\xff\xfe");
        let args = ["--json", "agent", "create", "minted"];
        let mut cmd = base_command(home, &args);
        cmd.env("ENVELOPE_AGENT_TOKEN", bad);
        let (out, timed_out) = finish(cmd, Duration::from_secs(10));
        assert_refused(&out, timed_out, "agent_token_invalid", &args);

        // MCP never falls back to anonymous for a token it cannot read.
        let mut cmd = base_command(home, &["mcp"]);
        cmd.env("ENVELOPE_AGENT_TOKEN", bad)
            .env("ENVELOPE_MCP_UNSAFE_ALLOW_ANONYMOUS", "1");
        let (out, timed_out) = finish(cmd, Duration::from_secs(10));
        assert!(!timed_out, "MCP started with an unreadable token");
        assert!(!out.status.success());
    }
    assert_eq!(
        count(home, "SELECT COUNT(*) FROM agent_identities"),
        1,
        "no agent was minted"
    );
}

// ── The accounts and folders a policy names ─────────────────────────

const ALPHA: &str = "alpha@example.test";
const BETA: &str = "beta@example.test";

/// `*` plus every named grant, so only the account and folder lists refuse.
const EVERY_ACTION: &str =
    "*,rules.write,rules.webhook,rules.batch_ack,sieve.publish,watch.webhook,unsubscribe";

fn account_id(home: &Path, email: &str) -> String {
    db(home)
        .conn()
        .query_row(
            "SELECT id FROM accounts WHERE username = ?1",
            [email],
            |r| r.get(0),
        )
        .expect("account id")
}

fn set_policy(home: &Path, name: &str, flags: &[&str]) {
    let mut args = vec!["agent", "policy", "set", name];
    args.extend_from_slice(flags);
    let out = run(home, &args);
    assert!(
        out.status.success(),
        "policy set failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A snooze on `account` for `uid`, due long ago and waiting for a reply.
fn seed_due_snooze(home: &Path, account: &str, uid: u32, from_folder: &str) {
    db(home)
        .create_snoozed(
            account,
            uid,
            from_folder,
            "Snoozed",
            "2000-01-01T00:00:00",
            Some("<snoozed@example.test>"),
            Some("waiting"),
            Some("waiting-reply"),
            None,
            Some("friend@example.test"),
        )
        .expect("seed snooze");
}

/// ALPHA and BETA, each behind its own probe, and the agent `skippy` with
/// the default policy. BETA has an operator rule, a draft, a scheduled
/// draft, and a snooze that is due and waits for a reply, so every gated
/// command has something there to act on.
struct TwoAccounts {
    _temp: tempfile::TempDir,
    alpha: MailProbe,
    beta: MailProbe,
    token: String,
    beta_draft: String,
    beta_scheduled: String,
}

impl TwoAccounts {
    fn home(&self) -> &Path {
        self._temp.path()
    }

    fn connections(&self) -> usize {
        self.alpha.connections() + self.beta.connections()
    }
}

fn two_accounts() -> TwoAccounts {
    let temp = tempfile::tempdir().expect("temp HOME");
    let home = temp.path();
    let (alpha, beta) = (MailProbe::start(), MailProbe::start());
    seed_account(home, ALPHA, &alpha);
    seed_account(home, BETA, &beta);
    let rule = run(
        home,
        &[
            "rule",
            "create",
            "--name",
            "archive",
            "--match-from",
            "*@sender.example",
            "--action",
            "move=Archive",
            "--disabled",
            "--account",
            BETA,
        ],
    );
    assert!(rule.status.success(), "operator rule create failed");
    let beta_id = account_id(home, BETA);
    let draft = |subject| {
        db(home)
            .create_draft(
                &beta_id,
                "a@b.test",
                Some(subject),
                Some("x"),
                None,
                None,
                None,
                None,
                Some("cli"),
            )
            .expect("create draft")
            .id
    };
    let beta_draft = draft("draft");
    let beta_scheduled = draft("scheduled");
    db(home)
        .update_draft_send_after(&beta_scheduled, "2030-01-01T09:00:00")
        .expect("schedule draft");
    seed_due_snooze(home, BETA, 1, "INBOX");
    let token = create_token(home, "skippy");
    TwoAccounts {
        _temp: temp,
        alpha,
        beta,
        token,
        beta_draft,
        beta_scheduled,
    }
}

/// Whether the command printed a refusal instead of running.
fn was_refused(out: &Output) -> bool {
    serde_json::from_slice::<Value>(&out.stdout).is_ok_and(|payload| payload["status"] == "denied")
}

/// [`snapshot`] plus the drafts, snoozes, tags and events a mailbox command
/// could change.
fn mailbox_snapshot(home: &Path) -> Vec<i64> {
    let mut rows = snapshot(home);
    rows.extend(
        [
            "SELECT COUNT(*) FROM drafts WHERE status = 'draft'",
            "SELECT COUNT(*) FROM drafts WHERE send_after IS NOT NULL",
            "SELECT COUNT(*) FROM snoozed",
            "SELECT COALESCE(SUM(escalation_tier + reply_received), 0) FROM snoozed",
            "SELECT COUNT(*) FROM message_tags",
            "SELECT COUNT(*) FROM events",
        ]
        .iter()
        .map(|sql| count(home, sql)),
    );
    rows
}

/// Every gated command, aimed at BETA, as `(contract key, arguments)`.
fn gated_commands_on_beta(t: &TwoAccounts) -> Vec<(&'static str, Vec<String>)> {
    let hook = "https://example.invalid/hook";
    let draft = t.beta_draft.as_str();
    let scheduled = t.beta_scheduled.as_str();
    let on_beta = |args: &[&str]| {
        args.iter()
            .copied()
            .chain(["--account", BETA])
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let as_is = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    vec![
        (
            "send",
            on_beta(&[
                "send",
                "--to",
                "a@b.test",
                "--subject",
                "hi",
                "--body",
                "x",
                "--attr",
                "informational",
            ]),
        ),
        (
            "draft send",
            as_is(&["draft", "send", draft, "--attr", "informational"]),
        ),
        ("move", on_beta(&["move", "1", "--to-folder", "Archive"])),
        ("copy", on_beta(&["copy", "1", "--to-folder", "Archive"])),
        ("delete", on_beta(&["delete", "1"])),
        ("flag add", on_beta(&["flag", "add", "1", "seen"])),
        ("flag remove", on_beta(&["flag", "remove", "1", "seen"])),
        (
            "bulk move",
            on_beta(&["bulk", "move", "--to-folder", "Archive", "--uids", "1"]),
        ),
        (
            "bulk copy",
            on_beta(&["bulk", "copy", "--to-folder", "Archive", "--uids", "1"]),
        ),
        (
            "bulk flag",
            on_beta(&[
                "bulk", "flag", "--flag", "seen", "--action", "add", "--uids", "1",
            ]),
        ),
        (
            "bulk delete",
            on_beta(&["bulk", "delete", "--uids", "1", "--confirm"]),
        ),
        (
            "bulk tag",
            on_beta(&["bulk", "tag", "--tag", "x", "--uids", "1"]),
        ),
        (
            "draft create",
            on_beta(&["draft", "create", "--to", "a@b.test"]),
        ),
        ("draft reply", on_beta(&["draft", "reply", "1"])),
        ("draft forward", on_beta(&["draft", "forward", "1"])),
        ("threat report", on_beta(&["threat", "report", "1"])),
        (
            "draft edit",
            on_beta(&["draft", "edit", draft, "--body", "y"]),
        ),
        ("draft discard", as_is(&["draft", "discard", draft])),
        ("scheduled hold", as_is(&["scheduled", "hold", scheduled])),
        (
            "scheduled cancel",
            as_is(&["scheduled", "cancel", scheduled]),
        ),
        (
            "snooze set",
            on_beta(&["snooze", "set", "1", "--until", "tomorrow"]),
        ),
        (
            "snooze check-replies",
            on_beta(&["snooze", "check-replies"]),
        ),
        ("snooze cancel", on_beta(&["snooze", "cancel", "1"])),
        ("unsnooze", on_beta(&["unsnooze", "--once"])),
        ("tag set", on_beta(&["tag", "set", "1", "--tag", "x"])),
        (
            "rule create",
            on_beta(&[
                "rule",
                "create",
                "--name",
                "everything",
                "--match-from",
                "*",
                "--action",
                "delete",
            ]),
        ),
        ("rule enable", on_beta(&["rule", "enable", "archive"])),
        ("rule disable", on_beta(&["rule", "disable", "archive"])),
        ("rule delete", on_beta(&["rule", "delete", "archive"])),
        ("rule run --confirm", on_beta(&["rule", "run", "--confirm"])),
        (
            "rule publish-sieve --confirm",
            on_beta(&["rule", "publish-sieve", "--confirm"]),
        ),
        (
            "unsubscribe --confirm",
            on_beta(&["unsubscribe", "1", "--confirm", "--attr", "informational"]),
        ),
        ("watch --webhook", on_beta(&["watch", "--webhook", hook])),
        ("watch --run-rules", on_beta(&["watch", "--run-rules"])),
        (
            "watch --webhook --run-rules",
            on_beta(&["watch", "--webhook", hook, "--run-rules"]),
        ),
        ("watch --deliver", on_beta(&["watch", "--deliver"])),
        (
            "watch --deliver --run-rules",
            on_beta(&["watch", "--deliver", "--run-rules"]),
        ),
    ]
}

#[test]
fn every_gated_command_checks_the_account_it_acts_on() {
    let t = two_accounts();
    let home = t.home();
    set_policy(
        home,
        "skippy",
        &["--allow-accounts", ALPHA, "--allow-actions", EVERY_ACTION],
    );
    let cases = gated_commands_on_beta(&t);

    let contract = stdout_json(&run(home, &["contract"]));
    let published: std::collections::BTreeSet<String> =
        contract["agent_identity"]["cli_token_gates"]["gated_commands"]
            .as_object()
            .expect("cli_token_gates.gated_commands")
            .keys()
            .cloned()
            .collect();
    let covered = cases.iter().map(|(key, _)| key.to_string()).collect();
    assert_eq!(published, covered, "give every gated command a case here");

    let before = mailbox_snapshot(home);
    let beta_id = account_id(home, BETA);
    let mut ran = Vec::new();
    for (key, args) in &cases {
        let args = with_json(args);
        let (out, timed_out) = run_as(home, &t.token, &args);
        if !was_refused(&out) {
            ran.push(*key);
            continue;
        }
        assert_refused(&out, timed_out, "agent_policy_denied_account", &args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains(BETA) && !stdout.contains(&beta_id),
            "`{key}`: the refusal names the account: {stdout}"
        );
    }
    assert!(
        ran.is_empty(),
        "these ran on BETA instead of being refused: {ran:?}"
    );

    assert_eq!(
        mailbox_snapshot(home),
        before,
        "a refused command changed state"
    );
    assert_eq!(
        t.connections(),
        0,
        "a refused command reached a mail server"
    );
    let recorded = count(
        home,
        &format!(
            "SELECT COUNT(*) FROM action_log WHERE action_status = 'denied' \
             AND justification = 'agent_policy_denied_account' AND account_id = '{beta_id}'"
        ),
    );
    assert_eq!(
        recorded,
        cases.len() as i64,
        "each refusal is in the action log under the account"
    );
}

#[test]
fn folder_limits_cover_the_source_and_the_destination() {
    let t = two_accounts();
    let home = t.home();
    set_policy(
        home,
        "skippy",
        &["--allow-folders", "INBOX", "--allow-actions", EVERY_ACTION],
    );
    // Snoozed out of Archive, so cancelling it or its return puts mail there.
    seed_due_snooze(home, ALPHA, 7, "Archive");
    let before = mailbox_snapshot(home);
    let mut ran = Vec::new();

    for args in [
        // Out of INBOX, into a folder the policy leaves out.
        vec!["move", "1", "--to-folder", "Archive"],
        vec!["copy", "1", "--to-folder", "Archive"],
        vec!["bulk", "move", "--to-folder", "Archive", "--uids", "1"],
        vec!["bulk", "copy", "--to-folder", "Archive", "--uids", "1"],
        // From a folder the policy leaves out, into INBOX.
        vec!["move", "1", "--folder", "Archive", "--to-folder", "INBOX"],
        vec!["copy", "1", "--folder", "Archive", "--to-folder", "INBOX"],
        vec![
            "bulk",
            "move",
            "--folder",
            "Archive",
            "--to-folder",
            "INBOX",
            "--uids",
            "1",
        ],
        // Acting on mail in a folder the policy leaves out.
        vec!["delete", "1", "--folder", "Archive"],
        vec!["flag", "add", "1", "seen", "--folder", "Archive"],
        vec!["flag", "remove", "1", "seen", "--folder", "Archive"],
        vec![
            "bulk", "flag", "--flag", "seen", "--action", "add", "--folder", "Archive", "--uids",
            "1",
        ],
        vec![
            "bulk",
            "delete",
            "--confirm",
            "--folder",
            "Archive",
            "--uids",
            "1",
        ],
        vec![
            "bulk", "tag", "--tag", "x", "--folder", "Archive", "--uids", "1",
        ],
        vec!["tag", "set", "1", "--tag", "x", "--folder", "Archive"],
        vec![
            "snooze", "set", "1", "--until", "tomorrow", "--folder", "Archive",
        ],
        vec!["snooze", "cancel", "7"],
        vec!["unsnooze", "--once"],
        vec!["draft", "reply", "1", "--folder", "Archive"],
        vec!["draft", "forward", "1", "--folder", "Archive"],
        vec!["threat", "report", "1", "--folder", "Archive"],
        vec!["rule", "run", "--confirm", "--folder", "Archive"],
        vec![
            "unsubscribe",
            "1",
            "--confirm",
            "--attr",
            "informational",
            "--folder",
            "Archive",
        ],
        vec!["watch", "--run-rules", "--folder", "Archive"],
        vec![
            "watch",
            "--webhook",
            "https://example.invalid/hook",
            "--folder",
            "Archive",
        ],
    ] {
        let mut args: Vec<&str> = std::iter::once("--json").chain(args).collect();
        args.extend_from_slice(&["--account", ALPHA]);
        let (out, timed_out) = run_as(home, &t.token, &args);
        if !was_refused(&out) {
            ran.push(args[1..].join(" "));
            continue;
        }
        assert_refused(&out, timed_out, "agent_policy_denied_folder", &args);
    }
    assert!(
        ran.is_empty(),
        "these ran instead of being refused: {ran:#?}"
    );
    assert_eq!(
        mailbox_snapshot(home),
        before,
        "a refused command changed state"
    );
    assert_eq!(
        t.connections(),
        0,
        "a refused command reached a mail server"
    );

    // Inside the list, the commands go on to the mail server.
    let allowed = |args: &[&str]| {
        let before = t.alpha.connections();
        let mut args: Vec<&str> = std::iter::once("--json")
            .chain(args.iter().copied())
            .collect();
        args.extend_from_slice(&["--account", ALPHA]);
        let (out, timed_out) = run_as(home, &t.token, &args);
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!timed_out, "{args:?} hung");
        assert!(!output.contains("\"denied\""), "{args:?}: {output}");
        assert!(
            t.alpha.connections() > before,
            "{args:?} went on to the mail server"
        );
    };
    allowed(&["flag", "add", "1", "seen"]);
    allowed(&["bulk", "delete", "--confirm", "--uids", "1"]);
    set_policy(home, "skippy", &["--allow-folders", "INBOX,Archive"]);
    allowed(&["move", "1", "--to-folder", "Archive"]);
    allowed(&["move", "1", "--folder", "Archive", "--to-folder", "INBOX"]);
}

#[test]
fn a_star_policy_and_the_operator_act_on_every_account_and_folder() {
    let t = two_accounts();
    let home = t.home();
    let cases: [&[&str]; 5] = [
        &["move", "1", "--folder", "Archive", "--to-folder", "Junk"],
        &["flag", "add", "1", "seen", "--folder", "Archive"],
        &["delete", "1", "--folder", "Archive"],
        &[
            "bulk",
            "move",
            "--folder",
            "Archive",
            "--to-folder",
            "Junk",
            "--uids",
            "1",
        ],
        &[
            "bulk",
            "delete",
            "--confirm",
            "--folder",
            "Archive",
            "--uids",
            "1",
        ],
    ];
    // `skippy` keeps the default policy: `*` accounts, folders and actions.
    for token in [Some(t.token.as_str()), None] {
        for case in cases {
            let before = t.beta.connections();
            let mut args: Vec<&str> = std::iter::once("--json")
                .chain(case.iter().copied())
                .collect();
            args.extend_from_slice(&["--account", BETA]);
            let (out, timed_out) = match token {
                Some(token) => run_as(home, token, &args),
                None => (run(home, &args), false),
            };
            let output = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(!timed_out, "{args:?} hung");
            assert!(
                !output.contains("\"denied\""),
                "{token:?} {args:?}: {output}"
            );
            assert!(
                t.beta.connections() > before,
                "{args:?} went on to BETA's mail server"
            );
        }
    }
}

#[test]
fn a_sweep_needs_every_account_it_reaches() {
    let t = two_accounts();
    let home = t.home();
    set_policy(home, "skippy", &["--allow-accounts", ALPHA]);
    seed_due_snooze(home, ALPHA, 7, "INBOX");
    let before = mailbox_snapshot(home);

    for args in [
        // BETA's snooze is due as well as ALPHA's.
        &["--json", "unsnooze", "--once"][..],
        // The reply search reads BETA's INBOX too.
        &["--json", "snooze", "check-replies", "--account", ALPHA],
    ] {
        let (out, timed_out) = run_as(home, &t.token, args);
        assert_refused(&out, timed_out, "agent_policy_denied_account", args);
    }
    assert_eq!(
        mailbox_snapshot(home),
        before,
        "a refused sweep changed state"
    );
    assert_eq!(t.connections(), 0, "a refused sweep reached a mail server");

    // Narrowed to ALPHA, the sweep goes on to ALPHA's mail server.
    let args = ["--json", "unsnooze", "--once", "--account", ALPHA];
    let (out, timed_out) = run_as(home, &t.token, &args);
    assert!(!timed_out, "{args:?} hung");
    assert!(
        !was_refused(&out),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(t.alpha.connections() > 0, "{args:?} reached ALPHA");
    assert_eq!(t.beta.connections(), 0, "{args:?} stayed off BETA");
}

#[test]
fn a_draft_is_checked_under_its_own_account() {
    let t = two_accounts();
    let home = t.home();
    set_policy(home, "skippy", &["--allow-accounts", ALPHA]);
    let before = mailbox_snapshot(home);
    let (draft, scheduled) = (t.beta_draft.as_str(), t.beta_scheduled.as_str());

    // ALPHA is allowed, and each id names one of BETA's drafts.
    for args in [
        &[
            "--json",
            "draft",
            "edit",
            draft,
            "--body",
            "y",
            "--account",
            ALPHA,
        ][..],
        &["--json", "draft", "discard", draft, "--account", ALPHA],
        &["--json", "scheduled", "hold", scheduled, "--account", ALPHA],
        &[
            "--json",
            "scheduled",
            "cancel",
            scheduled,
            "--account",
            ALPHA,
        ],
    ] {
        let (out, timed_out) = run_as(home, &t.token, args);
        assert_refused(&out, timed_out, "agent_policy_denied_account", args);
    }
    assert_eq!(
        mailbox_snapshot(home),
        before,
        "a refused command changed a draft"
    );
    assert_eq!(
        t.connections(),
        0,
        "a refused command reached a mail server"
    );
}
