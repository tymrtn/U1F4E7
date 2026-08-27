// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope governor catalog` — discover the vendored, weight-free Governor
//! attribution catalog.
//!
//! This is a pure, local projection: it never spawns Governor, never opens a
//! socket, and works even when the Governor binary is absent (which is exactly
//! when a bot most needs to understand a `governor_unavailable`). It exposes
//! public key/description/category/provenance and declaration guidance — never a
//! weight, threshold, or score.

use anyhow::Result;
use envelope_email_transport::attribution_provenance::provenance_of;
use envelope_email_transport::governor_catalog::{
    CATALOG_NAME, catalog_version, envelope_projection,
};

/// Print the vendored Envelope Governor catalog projection.
pub fn run_catalog(json: bool) -> Result<()> {
    let projection = envelope_projection();
    if json {
        println!("{}", serde_json::to_string_pretty(&projection)?);
        return Ok(());
    }

    println!(
        "Envelope Governor catalog `{CATALOG_NAME}` (v{}) — {} attributes",
        catalog_version(),
        projection["attributes"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0)
    );
    println!(
        "Declare factual keys on a send with `--attr <key>` (CLI) or the `attributes` array (MCP)."
    );
    println!();
    println!(
        "  {:<22} {:<12} {:<20} DESCRIPTION",
        "KEY", "CATEGORY", "PROVENANCE"
    );
    if let Some(attrs) = projection["attributes"].as_array() {
        for a in attrs {
            let key = a["key"].as_str().unwrap_or("");
            let category = a["category"].as_str().unwrap_or("");
            let provenance = provenance_of(key)
                .map(|p| p.as_str())
                .unwrap_or("host_derived");
            let description = a["description"].as_str().unwrap_or("");
            println!("  {key:<22} {category:<12} {provenance:<20} {description}");
        }
    }
    println!();
    println!("Rules:");
    if let Some(rules) = projection["rules"].as_array() {
        for r in rules {
            if let Some(s) = r.as_str() {
                println!("  - {s}");
            }
        }
    }
    Ok(())
}
