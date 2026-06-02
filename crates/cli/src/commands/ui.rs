// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Ambient dashboard UI metadata for agent-facing JSON outputs.
//!
//! Every CLI/MCP JSON response that has account/draft/message/rule context
//! should carry a `ui` object so Envelopie/Hermes/Codex can hand Tyler the
//! relevant dashboard URL without reconstructing it.
//!
//! Base URL precedence:
//!   1. `ENVELOPE_DASHBOARD_BASE_URL` environment variable
//!   2. `ENVELOPE_DASHBOARD_URL` backwards-compatible alias
//!   3. persistent `dashboard.base_url` config
//!   4. `http://localhost:3141`
//!
//! Helpers never emit secrets — only account ids, draft ids, message UIDs
//! and folder/query names go into URLs, and folder/draft values are
//! percent-encoded so embedded `?` / `/` / spaces don't break the path.

use serde::Serialize;
use serde_json::{Value, json};

/// Default dashboard origin when no dashboard base URL is configured.
pub const DEFAULT_DASHBOARD_BASE: &str = "http://localhost:3141";

/// Resolve the dashboard base URL (origin only, no trailing slash).
pub fn dashboard_base() -> String {
    super::config::resolved_dashboard_base_url().value
}

fn join(path: &str) -> String {
    let base = dashboard_base();
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

/// Percent-encode a string for safe placement inside a URL path segment.
///
/// Only RFC 3986 unreserved characters survive unencoded — every other byte
/// (including `/`, `?`, `&`, `#`, spaces, and any UTF-8 continuation byte)
/// is escaped as `%XX`. Used for account ids, draft ids, folder names, and
/// query values alike; over-encoding query characters is harmless.
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Root-level UI metadata when there is no account/draft/message context.
pub fn root_ui() -> Value {
    json!({
        "dashboard_url": dashboard_base(),
        "dashboard_path": "/",
    })
}

/// UI metadata anchored at an account's agent cockpit.
pub fn account_ui(account_id: &str) -> Value {
    let acct = encode_segment(account_id);
    let cockpit_path = format!("/accounts/{acct}/cockpit");
    json!({
        "dashboard_url": dashboard_base(),
        "dashboard_path": cockpit_path.clone(),
        "cockpit_url": join(&cockpit_path),
    })
}

/// UI metadata for a specific draft: `review_url` points at the draft
/// approval surface inside the cockpit.
pub fn draft_ui(account_id: &str, draft_id: &str) -> Value {
    let acct = encode_segment(account_id);
    let draft = encode_segment(draft_id);
    let draft_path = format!("/accounts/{acct}/drafts/{draft}");
    let cockpit_path = format!("/accounts/{acct}/cockpit");
    json!({
        "dashboard_url": dashboard_base(),
        "dashboard_path": draft_path.clone(),
        "cockpit_url": join(&cockpit_path),
        "review_url": join(&draft_path),
    })
}

/// UI metadata for rule-related responses; `rules_url` is the rules panel.
pub fn rules_ui(account_id: &str) -> Value {
    let acct = encode_segment(account_id);
    let rules_path = format!("/accounts/{acct}/rules");
    let cockpit_path = format!("/accounts/{acct}/cockpit");
    json!({
        "dashboard_url": dashboard_base(),
        "dashboard_path": rules_path.clone(),
        "cockpit_url": join(&cockpit_path),
        "rules_url": join(&rules_path),
    })
}

/// UI metadata for a specific message; `message_url` includes the folder
/// as a query parameter so the dashboard can re-resolve the UID.
pub fn message_ui(account_id: &str, uid: u32, folder: &str) -> Value {
    let acct = encode_segment(account_id);
    let folder_enc = encode_segment(folder);
    let msg_path = format!("/accounts/{acct}/messages/{uid}?folder={folder_enc}");
    let cockpit_path = format!("/accounts/{acct}/cockpit");
    json!({
        "dashboard_url": dashboard_base(),
        "dashboard_path": msg_path.clone(),
        "cockpit_url": join(&cockpit_path),
        "message_url": join(&msg_path),
    })
}

/// In-place: attach `ui` to a JSON object. Non-objects are left unchanged.
pub fn attach_ui(value: &mut Value, ui: Value) {
    if let Value::Object(map) = value {
        map.insert("ui".to_string(), ui);
    }
}

/// Serialize `item` to JSON and attach a `ui` field. Falls back to the raw
/// value if the result isn't an object (so arrays/strings aren't wrapped).
pub fn with_ui<T: Serialize>(item: &T, ui: Value) -> Value {
    let mut value = serde_json::to_value(item).unwrap_or(Value::Null);
    attach_ui(&mut value, ui);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_config_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "envelope-ui-test-{}-{name}.json",
            std::process::id()
        ))
    }

    fn isolated_dashboard_config(name: &str) -> crate::commands::config::DashboardConfigTestGuard {
        let path = test_config_path(name);
        let _ = std::fs::remove_file(&path);
        crate::commands::config::isolated_dashboard_config(path)
    }

    #[test]
    fn dashboard_base_defaults_to_localhost() {
        let _guard = isolated_dashboard_config("default");
        assert_eq!(dashboard_base(), "http://localhost:3141");
    }

    #[test]
    fn dashboard_base_strips_trailing_slash() {
        let guard = isolated_dashboard_config("trailing-slash");
        guard.set_primary_env("http://example.test:8080/");
        assert_eq!(dashboard_base(), "http://example.test:8080");
    }

    #[test]
    fn dashboard_base_prefers_primary_env_over_alias() {
        let guard = isolated_dashboard_config("primary-env");
        guard.set_alias_env("https://alias.example.test/");
        guard.set_primary_env("https://primary.example.test/");

        assert_eq!(dashboard_base(), "https://primary.example.test");
    }

    #[test]
    fn dashboard_base_uses_alias_when_primary_env_is_absent() {
        let guard = isolated_dashboard_config("alias-env");
        guard.set_alias_env("https://alias.example.test/");

        assert_eq!(dashboard_base(), "https://alias.example.test");
    }

    #[test]
    fn encode_segment_escapes_unsafe_bytes() {
        // Slash, question mark, ampersand, space, and UTF-8 must be escaped.
        assert_eq!(encode_segment("INBOX"), "INBOX");
        assert_eq!(encode_segment("Sent Items"), "Sent%20Items");
        assert_eq!(encode_segment("foo/bar"), "foo%2Fbar");
        assert_eq!(encode_segment("a?b&c"), "a%3Fb%26c");
        assert_eq!(encode_segment("Café"), "Caf%C3%A9");
    }

    #[test]
    fn account_ui_includes_cockpit_url() {
        let _guard = isolated_dashboard_config("account-default");
        let ui = account_ui("acct-1");
        assert_eq!(ui["dashboard_url"], "http://localhost:3141");
        assert_eq!(ui["dashboard_path"], "/accounts/acct-1/cockpit");
        assert_eq!(
            ui["cockpit_url"],
            "http://localhost:3141/accounts/acct-1/cockpit"
        );
    }

    #[test]
    fn account_ui_uses_configured_base_url_for_cockpit_url() {
        let guard = isolated_dashboard_config("account-configured-base");
        guard.set_primary_env("https://dash.example.test/envelope/");

        let ui = account_ui("acct/one");
        assert_eq!(ui["dashboard_url"], "https://dash.example.test/envelope");
        assert_eq!(ui["dashboard_path"], "/accounts/acct%2Fone/cockpit");
        assert_eq!(
            ui["cockpit_url"],
            "https://dash.example.test/envelope/accounts/acct%2Fone/cockpit"
        );
    }

    #[test]
    fn draft_ui_includes_review_url() {
        let _guard = isolated_dashboard_config("draft-default");
        let ui = draft_ui("acct-1", "draft-abc");
        assert_eq!(
            ui["review_url"],
            "http://localhost:3141/accounts/acct-1/drafts/draft-abc"
        );
        assert_eq!(
            ui["cockpit_url"],
            "http://localhost:3141/accounts/acct-1/cockpit"
        );
    }

    #[test]
    fn rules_ui_includes_rules_url() {
        let _guard = isolated_dashboard_config("rules-default");
        let ui = rules_ui("acct-1");
        assert_eq!(
            ui["rules_url"],
            "http://localhost:3141/accounts/acct-1/rules"
        );
    }

    #[test]
    fn message_ui_escapes_folder_and_keeps_uid() {
        let _guard = isolated_dashboard_config("message-default");
        let ui = message_ui("acct-1", 42, "Sent Items");
        assert_eq!(
            ui["message_url"],
            "http://localhost:3141/accounts/acct-1/messages/42?folder=Sent%20Items"
        );
    }

    #[test]
    fn message_ui_uses_configured_base_url_and_preserves_query_encoding() {
        let guard = isolated_dashboard_config("message-configured-base");
        guard.set_primary_env("https://dash.example.test/envelope/");

        let ui = message_ui("acct/one", 42, "Sent/Items & Stuff");
        assert_eq!(
            ui["message_url"],
            "https://dash.example.test/envelope/accounts/acct%2Fone/messages/42?folder=Sent%2FItems%20%26%20Stuff"
        );
        assert_eq!(
            ui["cockpit_url"],
            "https://dash.example.test/envelope/accounts/acct%2Fone/cockpit"
        );
    }

    #[test]
    fn attach_ui_only_touches_objects() {
        let mut obj = json!({"x": 1});
        attach_ui(&mut obj, json!({"dashboard_url": "x"}));
        assert!(obj.get("ui").is_some());

        let mut arr = json!([1, 2, 3]);
        attach_ui(&mut arr, json!({"dashboard_url": "x"}));
        assert!(arr.get("ui").is_none());
    }

    #[test]
    fn with_ui_preserves_existing_fields() {
        #[derive(Serialize)]
        struct Item {
            id: String,
        }
        let v = with_ui(&Item { id: "abc".into() }, json!({"dashboard_url": "x"}));
        assert_eq!(v["id"], "abc");
        assert_eq!(v["ui"]["dashboard_url"], "x");
    }
}
