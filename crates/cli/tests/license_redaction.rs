// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::path::Path;
use std::process::Command;

fn envelope_bin() -> &'static str {
    env!("CARGO_BIN_EXE_envelope")
}

fn run_envelope(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(envelope_bin())
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run envelope")
}

#[test]
fn license_activate_never_echoes_key_material() {
    let temp = tempfile::tempdir().expect("temp HOME");
    let secret = "env_live_secret_license_key_do_not_log";

    for args in [
        vec!["license", "activate", secret],
        vec!["--json", "license", "activate", secret],
    ] {
        let output = run_envelope(temp.path(), &args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            !output.status.success(),
            "activation is not implemented yet"
        );
        assert!(
            !stdout.contains(secret),
            "stdout leaked license key for args {args:?}: {stdout}"
        );
        assert!(
            !stderr.contains(secret),
            "stderr leaked license key for args {args:?}: {stderr}"
        );
    }
}
