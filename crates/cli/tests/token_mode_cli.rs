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
