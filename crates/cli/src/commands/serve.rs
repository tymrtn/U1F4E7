// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::Result;

#[tokio::main]
pub async fn run(port: u16, no_background_sweeps: bool) -> Result<()> {
    let options = if no_background_sweeps {
        envelope_email_dashboard::ServeOptions::without_background_sweeps()
    } else {
        envelope_email_dashboard::ServeOptions::default()
    };

    envelope_email_dashboard::serve_with_options(port, options).await
}
