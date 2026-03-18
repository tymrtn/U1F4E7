// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::Result;

#[tokio::main]
pub async fn run(port: u16) -> Result<()> {
    envelope_email_dashboard::serve(port).await
}
