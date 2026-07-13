// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Secret input that never places credentials in process arguments.

use anyhow::{Context, Result, bail};
use std::io::{self, IsTerminal};

/// Read a secret either from a hidden terminal prompt or one stdin line.
///
/// Automation must opt into `--*-stdin`; this prevents a non-interactive
/// invocation from silently consuming an unrelated pipe as a credential.
pub fn read_secret(label: &str, from_stdin: bool) -> Result<String> {
    let value = if from_stdin {
        let mut value = String::new();
        io::stdin()
            .read_line(&mut value)
            .context("failed to read secret from stdin")?;
        value.trim_end_matches(['\n', '\r']).to_string()
    } else {
        if !io::stdin().is_terminal() {
            bail!(
                "{label} must be supplied through stdin in non-interactive mode; \
                 pass the corresponding --*-stdin flag"
            );
        }
        rpassword::prompt_password(format!("{label}: "))
            .context("failed to read secret from terminal")?
    };

    if value.is_empty() {
        bail!("{label} cannot be empty");
    }

    Ok(value)
}
