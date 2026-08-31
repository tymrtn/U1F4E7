// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::net::IpAddr;

use anyhow::Result;
use envelope_email_dashboard::ServeConfig;
use envelope_email_dashboard::auth::AuthConfig;

use crate::commands::config;

/// The port `serve` listens on by default, and the one the documented
/// `tailscale serve` remote setup fronts.
const DEFAULT_DASHBOARD_PORT: u16 = 3141;

#[tokio::main]
pub async fn run(port: u16, bind: IpAddr, no_background_sweeps: bool, no_auth: bool) -> Result<()> {
    let options = if no_background_sweeps {
        envelope_email_dashboard::ServeOptions::without_background_sweeps()
    } else {
        envelope_email_dashboard::ServeOptions::default()
    };

    let (auth, warning) = resolve_auth(
        no_auth,
        port,
        config::resolved_dashboard_auth_token(),
        config::resolved_dashboard_tailscale_allow(),
    )?;
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }

    envelope_email_dashboard::serve_with_config(ServeConfig {
        port,
        bind,
        options,
        auth,
        ..ServeConfig::default()
    })
    .await
}

/// Merge env + persisted config (env wins) into the auth policy the dashboard
/// will enforce; empty means open loopback mode. Returns the policy plus an
/// operator warning to print before startup.
///
/// `--no-auth` drops that policy. "Loopback" is not the same as "private":
/// the documented remote setup is `tailscale serve` in front of loopback
/// 3141, and auth.rs enforces on *configuration* rather than bind address
/// for exactly that reason. Dropping the policy on the fronted port would
/// publish the mailbox to the tailnet with no credential, so refuse it.
/// The desktop shell is unaffected — it always runs on an ephemeral port.
fn resolve_auth(
    no_auth: bool,
    port: u16,
    configured_token: Option<String>,
    configured_allow: Vec<String>,
) -> Result<(AuthConfig, Option<String>)> {
    let configured = AuthConfig::from_parts(configured_token, configured_allow);
    if !no_auth {
        return Ok((configured, None));
    }

    if configured.is_enforced() && port == DEFAULT_DASHBOARD_PORT {
        anyhow::bail!(
            "refusing --no-auth on the default dashboard port ({DEFAULT_DASHBOARD_PORT}). \
             Dashboard auth is configured, and this is the port the documented \
             `tailscale serve` setup fronts — running unauthenticated here would expose \
             every mailbox to the tailnet with no credential. --no-auth exists for a \
             private, ephemeral listener (the desktop shell picks one per launch); pass \
             an explicit --port for that, or drop --no-auth to keep the configured policy."
        );
    }

    let warning = configured.is_enforced().then(|| {
        format!(
            "warning: --no-auth is ignoring the configured dashboard auth on port {port}. \
             Anything that can reach this port can read and send mail without a \
             credential. Make sure nothing is proxying it."
        )
    });

    Ok((AuthConfig::disabled(), warning))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn token() -> Option<String> {
        Some("s3cr3t".to_string())
    }

    #[test]
    fn no_auth_on_default_port_with_configured_token_is_refused() {
        let err = resolve_auth(true, DEFAULT_DASHBOARD_PORT, token(), vec![])
            .expect_err("configured auth on the tailscale-fronted port must refuse --no-auth");
        let msg = err.to_string();
        assert!(msg.contains("3141"), "error should name the port: {msg}");
        assert!(
            msg.contains("tailscale serve"),
            "error should explain the tailnet exposure: {msg}"
        );
        assert!(
            msg.contains("--port"),
            "error should point at the private-port escape hatch: {msg}"
        );
    }

    #[test]
    fn no_auth_on_default_port_with_identity_allowlist_is_refused() {
        let allow = vec!["op@tailnet.ts.net".to_string()];
        assert!(resolve_auth(true, DEFAULT_DASHBOARD_PORT, None, allow).is_err());
    }

    #[test]
    fn no_auth_on_explicit_loopback_port_disables_auth_and_warns() {
        let allow = vec!["op@tailnet.ts.net".to_string()];
        let (auth, warning) = resolve_auth(true, 49172, token(), allow)
            .expect("an explicit non-default port is the desktop shell's private case");
        assert!(!auth.is_enforced());
        let warning = warning.expect("dropping configured auth must warn");
        assert!(
            warning.contains("49172"),
            "warning should name the port: {warning}"
        );
    }

    #[test]
    fn no_auth_without_configured_auth_is_quiet_even_on_default_port() {
        let (auth, warning) = resolve_auth(true, DEFAULT_DASHBOARD_PORT, None, vec![])
            .expect("nothing configured means nothing is dropped");
        assert!(!auth.is_enforced());
        assert!(warning.is_none());
    }

    #[test]
    fn absent_flag_preserves_configured_auth() {
        let (auth, warning) =
            resolve_auth(false, DEFAULT_DASHBOARD_PORT, token(), vec![]).expect("default path");
        assert!(auth.is_enforced());
        assert!(warning.is_none());
    }

    #[test]
    fn absent_flag_without_config_stays_open_loopback() {
        let (auth, warning) =
            resolve_auth(false, DEFAULT_DASHBOARD_PORT, None, vec![]).expect("default path");
        assert!(!auth.is_enforced());
        assert!(warning.is_none());
    }

    #[tokio::test]
    async fn non_loopback_bind_remains_refused_with_no_auth() {
        let (auth, _) = resolve_auth(true, 49172, token(), vec![]).expect("private port");
        let err = envelope_email_dashboard::serve_with_config(ServeConfig {
            port: 49172,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            auth,
            ..ServeConfig::default()
        })
        .await
        .expect_err("--no-auth must never open a non-loopback listener");
        assert!(
            err.to_string().contains("refusing"),
            "unexpected error: {err}"
        );
    }
}
