// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Embed the `static/` directory into the binary at compile time.
//!
//! This lets `cargo install envelope-email` produce a single binary with
//! no runtime file dependencies — the dashboard HTML/CSS/JS ships inside
//! the executable.

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "static/"]
pub struct Assets;

impl Assets {
    pub fn get_file(path: &str) -> Option<Vec<u8>> {
        Self::get(path).map(|f| f.data.into_owned())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dashboard_static_assets_do_not_expose_stale_030_version_copy() {
        let index = include_str!("../static/index.html");
        let js = include_str!("../static/dashboard.js");

        assert!(
            !index.contains("v0.3.0"),
            "dashboard header must not hardcode v0.3.0"
        );
        assert!(
            !js.contains("v0.3.0"),
            "dashboard JS must not mention stale v0.3.0 copy"
        );
    }

    #[test]
    fn dashboard_static_assets_expose_rules_control_plane() {
        let index = include_str!("../static/index.html");
        let js = include_str!("../static/dashboard.js");
        let lib = include_str!("lib.rs");

        assert!(
            index.contains("Rules Control Plane"),
            "dashboard should expose rules as a first-class operator surface"
        );
        assert!(
            index.contains("btn-refresh-rules"),
            "dashboard needs an explicit rule refresh control"
        );
        assert!(
            index.contains("btn-reader-test-rules"),
            "message reader should let humans dry-run rules against the selected message"
        );
        assert!(
            index.contains("btn-run-rules"),
            "rules control plane should expose bounded run-now controls"
        );
        assert!(
            index.contains("rules-run-limit"),
            "rules run controls should require an explicit bounded limit"
        );
        assert!(
            js.contains("/rules/run"),
            "dashboard JS should call the rules run API endpoint"
        );
        assert!(
            js.contains("runEnabledRulesForCurrentFolder"),
            "dashboard JS should bind a folder-aware rule-run workflow"
        );
        assert!(
            js.contains("Math.min(200"),
            "dashboard JS should clamp dashboard rule runs to a 200-message safety limit"
        );
        assert!(
            lib.contains("/accounts/{id}/rules/run")
                && lib.contains("handlers::rules::run_enabled"),
            "dashboard router should wire the rules run handler"
        );
        assert!(
            js.contains("loadRules"),
            "dashboard JS should fetch and render rules"
        );
        assert!(
            js.contains("testRulesForCurrentMessage"),
            "dashboard JS should dry-run rules for the selected message"
        );
    }
}
