// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use std::net::IpAddr;

use anyhow::Result;
use envelope_email_dashboard::ServeConfig;
use envelope_email_dashboard::auth::AuthConfig;

use crate::commands::config;

#[tokio::main]
pub async fn run(port: u16, bind: IpAddr, no_background_sweeps: bool) -> Result<()> {
    let options = if no_background_sweeps {
        envelope_email_dashboard::ServeOptions::without_background_sweeps()
    } else {
        envelope_email_dashboard::ServeOptions::default()
    };

    // Merge env + persisted config (env wins) into the auth policy the dashboard
    // will enforce. Empty means open loopback mode.
    let auth = AuthConfig::from_parts(
        config::resolved_dashboard_auth_token(),
        config::resolved_dashboard_tailscale_allow(),
    );

    envelope_email_dashboard::serve_with_config(ServeConfig {
        port,
        bind,
        options,
        auth,
        ..ServeConfig::default()
    })
    .await
}
