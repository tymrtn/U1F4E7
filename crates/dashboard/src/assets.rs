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

/// The Envelope v2 webmail SPA, compiled from `web/` by SvelteKit +
/// adapter-static and committed under `web/build/`. Embedded the same way as
/// [`Assets`] so `cargo install` never needs Node — the built bundle ships
/// inside the binary and is served by the axum `/v2` mount (see `lib.rs`).
#[derive(RustEmbed)]
#[folder = "web/build/"]
pub struct WebAssets;

impl WebAssets {
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
        assert!(
            js.contains("previewRuleBlastRadius") && js.contains("renderRulePreviewResult"),
            "rules control plane should explicitly preview a rule's blast radius before live runs"
        );
        assert!(
            js.contains("mutated=false") && js.contains("preview-sample-link"),
            "preview rendering must label non-mutation proof and expose message sample links"
        );
        assert!(
            js.contains("ruleActionRisk") && js.contains("high-risk"),
            "high-risk rule actions need stronger visual warnings before live run"
        );
        assert!(
            lib.contains("/accounts/{id}/rules/{rule_id}/preview")
                && lib.contains("handlers::rules::preview"),
            "dashboard router should wire a non-mutating rule preview endpoint"
        );
    }

    #[test]
    fn dashboard_static_assets_expose_unified_inbox_default_scope() {
        let index = include_str!("../static/index.html");
        let js = include_str!("../static/dashboard.js");
        let lib = include_str!("lib.rs");

        assert!(
            js.contains("Unified Inbox"),
            "dashboard account switcher should expose the merged inbox scope"
        );
        assert!(
            js.contains("/messages/unified"),
            "dashboard JS should fetch the unified inbox endpoint"
        );
        assert!(
            js.contains("selectUnifiedInbox"),
            "dashboard should be able to switch back to the unified inbox"
        );
        assert!(
            js.contains("account-scope"),
            "unified message rows should render account/folder identity"
        );
        assert!(
            index.contains("reader-account"),
            "reader should preserve visible mailbox context after opening unified rows"
        );
        assert!(
            lib.contains("/messages/unified") && lib.contains("handlers::messages::unified_inbox"),
            "dashboard router should wire the unified inbox endpoint"
        );
    }

    #[test]
    fn dashboard_static_assets_expose_read_state_and_thread_context() {
        let index = include_str!("../static/index.html");
        let js = include_str!("../static/dashboard.js");
        let css = include_str!("../static/dashboard.css");

        assert!(
            js.contains("isMessageUnread") && js.contains("read-toggle"),
            "message rows should derive unread state from flags and expose an explicit toggle"
        );
        assert!(
            css.contains(".msg-row.unseen") && css.contains(".read-toggle.unread"),
            "unread rows need visible bold/dot styling"
        );
        assert!(
            index.contains("reader-read-state") && js.contains("opening does not mark read"),
            "opening a message should show read state without implying passive mutation"
        );
        assert!(
            index.contains("reader-thread-row")
                && js.contains("threadMetaText")
                && js.contains("threadContextUrl"),
            "dashboard should display thread metadata and link to thread context"
        );
    }

    #[test]
    fn dashboard_static_assets_support_cli_deep_links() {
        let js = include_str!("../static/dashboard.js");
        let lib = include_str!("lib.rs");

        assert!(
            js.contains("parseDashboardRoute"),
            "dashboard JS should parse CLI/MCP deep-link paths"
        );
        assert!(
            js.contains("/accounts\\/([^/]+)\\/messages\\/(\\d+)"),
            "dashboard JS should recognize account-scoped message links"
        );
        assert!(
            js.contains("applyDashboardRoute"),
            "dashboard boot should apply parsed deep links after accounts load"
        );
        assert!(
            js.contains("await openMessage(route.uid, route.accountId"),
            "message deep links should open the reader for the target UID"
        );
        assert!(
            lib.contains("/accounts/{id}/messages/{uid}")
                && lib.contains("/accounts/{id}/cockpit")
                && lib.contains("/accounts/{id}/rules")
                && lib.contains("/accounts/{id}/drafts/{draft_id}"),
            "dashboard router should serve the SPA shell for CLI/MCP UI deep links"
        );

        // Route-first boot regression guards
        assert!(
            js.contains("autoSelect"),
            "loadAccounts must accept an autoSelect option to skip eager account/inbox selection"
        );
        assert!(
            js.contains("loadAccounts({ autoSelect: false })"),
            "route-first boot must call loadAccounts with autoSelect disabled for deep links"
        );
        assert!(
            js.contains("const route = parseDashboardRoute()"),
            "boot must parse the deep-link route before deciding the account load strategy"
        );
        assert!(
            !js.contains("await openMessage(route.uid, route.accountId, route.folder || 'INBOX');\n    loadRules();"),
            "message deep links must open the target message before any rules/folders/messages/cockpit hydration"
        );
        assert!(
            !js.contains("await loadStats();\n  await loadAccounts();\n  const routed = await applyDashboardRoute()"),
            "boot must not block on loadStats + auto-selecting loadAccounts before applying the deep route"
        );
    }

    #[test]
    fn dashboard_static_assets_render_operator_event_buckets() {
        let index = include_str!("../static/index.html");
        let js = include_str!("../static/dashboard.js");

        assert!(
            index.contains("Operator event buckets"),
            "cockpit should label events as operator-filtered buckets, not raw audit telemetry"
        );
        assert!(
            js.contains("Needs attention")
                && js.contains("Mailbox/watch events")
                && js.contains("Recent agent actions"),
            "dashboard JS should render the three operator event buckets"
        );
        assert!(
            js.contains("routine audit event") && js.contains("Audit Log/debug filter"),
            "routine audit telemetry should be hidden behind audit/debug copy"
        );
        assert!(
            js.contains("account_label")
                && js.contains("actor")
                && js.contains("outcome")
                && js.contains("message_link")
                && js.contains("ack_state"),
            "event rows should render account, actor, outcome, message link, and ack state"
        );
        assert!(
            js.contains("Create an OTP watch") && js.contains("Review pending drafts"),
            "empty event buckets should offer useful operator next steps"
        );
    }
}
