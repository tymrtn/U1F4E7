// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Snooze due checks through the real binary, in a temporary home, with the
//! child's local zone set by `TZ`. `return_at` is stored as naive UTC, so a
//! snooze an hour ahead in UTC is not due and one an hour past is, whatever
//! the local zone. No account is configured, so no mailbox is touched: a due
//! snooze shows up as one `unsnooze` could not return, and `check-replies`
//! settles each follow-up without searching. The one account a test adds has
//! a mail server that never resolves.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use chrono::{Duration, Utc};
use envelope_email_store::Database;
use serde_json::{Value, json};

const EMAIL: &str = "me@corp.test";

/// One zone ahead of UTC and one behind it: a cutoff read in local time is
/// early in the first and late in the second.
const ZONES: [&str; 2] = ["Europe/Madrid", "America/New_York"];

fn run(home: &Path, tz: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_envelope"))
        .args(args)
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .env("ENVELOPE_MASTER_KEY", "snooze-cli-test-key")
        .env("TZ", tz)
        .env_remove("ENVELOPE_AGENT_TOKEN")
        .env_remove("ENVELOPE_MASTER_PASSPHRASE_FILE")
        .stdin(Stdio::null())
        .output()
        .expect("run envelope")
}

fn cli(home: &Path, tz: &str, args: &[&str]) -> Value {
    let out = run(home, tz, args);
    assert!(
        out.status.success(),
        "TZ={tz} {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("JSON on stdout")
}

/// A home holding one snooze that returns `from_now` away, in UTC, stored as
/// `envelope snooze set` stores it.
fn home_with_snooze(
    from_now: Duration,
    reason: Option<&str>,
    recipient: Option<&str>,
) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("envelope-email");
    std::fs::create_dir_all(&data).unwrap();
    let db = Database::open(&data.join("envelope.db")).unwrap();
    let return_at = (Utc::now() + from_now)
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    db.create_snoozed(
        EMAIL,
        7,
        "INBOX",
        "Snoozed",
        &return_at,
        Some("<snoozed-1@corp.test>"),
        Some("Quote"),
        reason,
        None,
        recipient,
    )
    .unwrap();
    home
}

#[test]
fn unsnooze_leaves_a_snooze_an_hour_ahead_in_utc() {
    for tz in ZONES {
        let home = home_with_snooze(Duration::hours(1), None, None);
        let swept = cli(home.path(), tz, &["unsnooze", "--once", "--json"]);
        assert_eq!(swept, json!({ "unsnoozed": [] }), "TZ={tz}");
    }
}

#[test]
fn unsnooze_picks_up_a_snooze_an_hour_past_in_utc() {
    for tz in ZONES {
        let home = home_with_snooze(Duration::hours(-1), None, None);
        let swept = cli(home.path(), tz, &["unsnooze", "--once", "--json"]);
        // Due, and with no account configured nothing could move it back.
        assert_eq!(swept["success"], 0, "TZ={tz}: {swept}");
        assert_eq!(swept["failed"], 1, "TZ={tz}: {swept}");
    }
}

#[test]
fn check_replies_calls_a_follow_up_overdue_only_past_its_utc_return_time() {
    for tz in ZONES {
        let ahead = home_with_snooze(
            Duration::hours(1),
            Some("waiting-reply"),
            Some("them@corp.test"),
        );
        let checked = cli(ahead.path(), tz, &["snooze", "check-replies", "--json"]);
        let row = &checked["results"][0];
        assert_eq!(row["status"], "no_reply_waiting", "TZ={tz}: {checked}");
        assert_eq!(row["overdue"], false, "TZ={tz}: {checked}");
        assert_eq!(row["escalation_tier"], 0, "TZ={tz}: {checked}");

        let past = home_with_snooze(
            Duration::hours(-1),
            Some("waiting-reply"),
            Some("them@corp.test"),
        );
        let checked = cli(past.path(), tz, &["snooze", "check-replies", "--json"]);
        let row = &checked["results"][0];
        assert_eq!(row["status"], "no_reply_overdue", "TZ={tz}: {checked}");
        assert_eq!(row["overdue"], true, "TZ={tz}: {checked}");
        assert_eq!(row["escalation_tier"], 1, "TZ={tz}: {checked}");
    }
}

/// Add the account [`EMAIL`] to `home`, with a mail server whose name never
/// resolves: anything that tries to reach it fails.
fn add_unreachable_account(home: &Path) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_envelope"))
        .args([
            "accounts",
            "add",
            "--skip-login-check",
            "--email",
            EMAIL,
            "--password-stdin",
            "--smtp-host",
            "smtp.corp.invalid",
            "--smtp-port",
            "587",
            "--imap-host",
            "imap.corp.invalid",
            "--imap-port",
            "993",
            "--json",
        ])
        .env("HOME", home)
        .env("ENVELOPE_HOME", home)
        .env("ENVELOPE_MASTER_KEY", "snooze-cli-test-key")
        .env_remove("ENVELOPE_AGENT_TOKEN")
        .env_remove("ENVELOPE_MASTER_PASSPHRASE_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn accounts add");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"pw\n")
        .expect("write password");
    let out = child.wait_with_output().expect("wait accounts add");
    assert!(
        out.status.success(),
        "accounts add failed\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A snooze whose message is gone is dropped with `snooze cancel --forget`,
/// which deletes the snooze and touches no mailbox: the account's server
/// never resolves, so the plain cancel fails and keeps the snooze, and the
/// forget still succeeds.
#[test]
fn snooze_cancel_forget_drops_the_snooze_without_touching_the_mailbox() {
    let home = home_with_snooze(Duration::hours(1), None, None);
    add_unreachable_account(home.path());
    let db_path = home.path().join("envelope-email").join("envelope.db");

    let args = ["snooze", "cancel", "7", "--account", EMAIL, "--json"];
    let cancel = run(home.path(), "UTC", &args);
    assert!(!cancel.status.success(), "the server can't be reached");
    let db = Database::open(&db_path).unwrap();
    assert_eq!(db.list_snoozed(None).unwrap().len(), 1, "kept");
    drop(db);

    let forgot = cli(home.path(), "UTC", &[&args[..], &["--forget"]].concat());

    assert_eq!(forgot["action"], "forget", "{forgot}");
    assert_eq!(forgot["uid"], 7, "{forgot}");
    assert_eq!(forgot["moved"], false, "{forgot}");
    let db = Database::open(&db_path).unwrap();
    assert!(
        db.list_snoozed(None).unwrap().is_empty(),
        "the snooze is gone"
    );
}
