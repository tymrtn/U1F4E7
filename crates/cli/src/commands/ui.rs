// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Ambient dashboard UI metadata for agent-facing JSON outputs.
//!
//! Every CLI/MCP JSON response that has account/draft/message/rule context
//! should carry a `ui` object so Envelopie/Hermes/Codex can hand Tyler the
//! relevant dashboard URL without reconstructing it.
//!
//! Agent-facing origins are discovered from an active local Tailscale Serve
//! route to Envelope's loopback dashboard. A stale configured hostname does not
//! prove the service is still reachable, so it is never used for these links.
//!
//! Helpers never emit secrets — only account ids, draft ids, message UIDs
//! and folder/query names go into URLs, and folder/draft values are
//! percent-encoded so embedded `?` / `/` / spaces don't break the path.

use envelope_email_store::Database;
use envelope_email_transport::provider;
use serde::Serialize;
use serde_json::{Value, json};
#[cfg(not(test))]
use std::io::Read;
#[cfg(not(test))]
use std::process::{Command, Stdio};
#[cfg(not(test))]
use std::sync::OnceLock;
#[cfg(not(test))]
use std::time::Duration;
use tracing::warn;
#[cfg(not(test))]
use wait_timeout::ChildExt;

/// Default dashboard origin when no verified Tailscale Serve route is active.
pub const DEFAULT_DASHBOARD_BASE: &str = "http://localhost:3141";
#[cfg(not(test))]
const TAILSCALE_STATUS_TIMEOUT: Duration = Duration::from_secs(2);
const DASHBOARD_LOOPBACK_PROXY: &str = "http://127.0.0.1:3141";

#[cfg(not(test))]
static DISCOVERED_DASHBOARD_ORIGIN: OnceLock<ResolvedDashboardOrigin> = OnceLock::new();

/// A verified dashboard origin and the bounded observability information used
/// to select it. The warning deliberately contains only safe DNS labels, never
/// a full tailnet DNS name or Tailscale identity.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedDashboardOrigin {
    base_url: String,
    dashboard_origin_source: &'static str,
    dashboard_origin_warning: Option<String>,
}

/// Resolve the dashboard origin once per CLI process. Only a live Tailscale
/// Serve route to Envelope's loopback listener can produce a non-local origin.
pub fn dashboard_base() -> String {
    dashboard_origin().base_url
}

fn dashboard_origin() -> ResolvedDashboardOrigin {
    #[cfg(test)]
    {
        // Unit tests exercise the pure parser below; never require a local
        // Tailscale installation or leak its machine-specific host into tests.
        return localhost_origin(None);
    }

    #[cfg(not(test))]
    DISCOVERED_DASHBOARD_ORIGIN
        .get_or_init(discover_dashboard_origin)
        .clone()
}

#[cfg(not(test))]
fn discover_dashboard_origin() -> ResolvedDashboardOrigin {
    let serve_status = tailscale_json(["serve", "status", "--json"]);
    let node_status = tailscale_json(["status", "--json"]);
    resolve_dashboard_origin(serve_status.as_deref(), node_status.as_deref())
}

#[cfg(not(test))]
fn tailscale_json<const N: usize>(args: [&str; N]) -> Option<String> {
    let mut child = Command::new("tailscale")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let status = match child.wait_timeout(TAILSCALE_STATUS_TIMEOUT).ok()? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    if !status.success() {
        return None;
    }

    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    Some(output)
}

/// Resolve dashboard origin from local CLI outputs only. A verified Serve route
/// remains authoritative even when it belongs to a different node hostname:
/// the route, not node identity, proves ingress to Envelope.
fn resolve_dashboard_origin(
    serve_status: Option<&str>,
    node_status: Option<&str>,
) -> ResolvedDashboardOrigin {
    let serve_origin = serve_status.and_then(parse_tailscale_serve_status);
    let self_dns_name = node_status.and_then(parse_tailscale_self_dns_name);

    match (serve_origin, self_dns_name) {
        (Some(base_url), Some(self_dns_name)) => {
            let serve_host = base_url
                .strip_prefix("https://")
                .expect("verified Tailscale origin always has HTTPS scheme");
            let dashboard_origin_warning = (serve_host != self_dns_name).then(|| {
                format!(
                    "Verified Tailscale Serve host `{}` differs from this node's Tailscale DNS name `{}`; using the Serve host because it is the active Envelope ingress.",
                    safe_dns_label(serve_host),
                    safe_dns_label(&self_dns_name),
                )
            });
            ResolvedDashboardOrigin {
                base_url,
                dashboard_origin_source: "tailscale_serve",
                dashboard_origin_warning,
            }
        }
        (Some(base_url), None) => ResolvedDashboardOrigin {
            base_url,
            dashboard_origin_source: "tailscale_serve",
            dashboard_origin_warning: None,
        },
        (None, Some(self_dns_name)) => localhost_origin(Some(format!(
            "Local Tailscale node `{}` is online but is not serving Envelope through a verified HTTPS root proxy; using localhost.",
            safe_dns_label(&self_dns_name),
        ))),
        (None, None) => localhost_origin(Some(
            "Tailscale dashboard-origin discovery was unavailable or returned malformed status; using localhost."
                .to_string(),
        )),
    }
}

fn localhost_origin(dashboard_origin_warning: Option<String>) -> ResolvedDashboardOrigin {
    ResolvedDashboardOrigin {
        base_url: DEFAULT_DASHBOARD_BASE.to_string(),
        dashboard_origin_source: "localhost_fallback",
        dashboard_origin_warning,
    }
}

/// Parse Tailscale Serve status without trusting any configured hostname.
///
/// Exactly one HTTPS Web listener must expose its root path directly to
/// Envelope's loopback dashboard. Unknown status shapes and ambiguous matches
/// fail closed so agent links never point at an unverified tailnet host.
pub(crate) fn parse_tailscale_serve_status(status: &str) -> Option<String> {
    let status: Value = serde_json::from_str(status).ok()?;
    let tcp = status.get("TCP")?.as_object()?;
    let web = status.get("Web")?.as_object()?;
    let mut origins = Vec::new();

    for (listener, config) in web {
        let Some((host, port)) = parse_https_listener(listener) else {
            continue;
        };
        if !tcp
            .get(&port.to_string())
            .and_then(|listener| listener.get("HTTPS"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(proxy) = config
            .get("Handlers")
            .and_then(Value::as_object)
            .and_then(|handlers| handlers.get("/"))
            .and_then(|handler| handler.get("Proxy"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if matches_dashboard_loopback_proxy(proxy) {
            origins.push(format!("https://{host}"));
        }
    }

    match origins.as_slice() {
        [origin] => Some(origin.clone()),
        _ => None,
    }
}

/// Read the local node DNS name from `tailscale status --json`. This accepts
/// only an ordinary DNS hostname so it cannot be reused to inject URL or
/// warning content.
pub(crate) fn parse_tailscale_self_dns_name(status: &str) -> Option<String> {
    let status: Value = serde_json::from_str(status).ok()?;
    let dns_name = status.get("Self")?.get("DNSName")?.as_str()?;
    normalize_dns_name(dns_name)
}

fn parse_https_listener(listener: &str) -> Option<(String, u16)> {
    let (host, port) = listener.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    if port != 443 {
        return None;
    }

    // Tailscale hostnames are DNS hostnames. Normalize case and the harmless
    // DNS trailing dot, while rejecting anything that could alter a URL origin.
    Some((normalize_dns_name(host)?, port))
}

fn normalize_dns_name(name: &str) -> Option<String> {
    let host = name.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host.split('.').any(|label| label.is_empty())
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return None;
    }
    Some(host)
}

fn safe_dns_label(host: &str) -> &str {
    host.split('.').next().unwrap_or("unknown")
}

fn matches_dashboard_loopback_proxy(proxy: &str) -> bool {
    proxy.strip_suffix('/').unwrap_or(proxy) == DASHBOARD_LOOPBACK_PROXY
}

fn join(base: &str, path: &str) -> String {
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

fn attach_dashboard_origin_metadata(ui: &mut Value, origin: &ResolvedDashboardOrigin) {
    let Value::Object(map) = ui else {
        return;
    };
    map.insert(
        "dashboard_origin_source".to_string(),
        Value::String(origin.dashboard_origin_source.to_string()),
    );
    if let Some(warning) = &origin.dashboard_origin_warning {
        map.insert(
            "dashboard_origin_warning".to_string(),
            Value::String(warning.clone()),
        );
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

/// The dashboard's cockpit client route. It is global, not account-scoped —
/// the SPA has no `/accounts/{id}/cockpit` route and renders its own 404 there.
const COCKPIT_PATH: &str = "/cockpit";

/// The dashboard's rules client route. Global for the same reason as
/// [`COCKPIT_PATH`].
const RULES_PATH: &str = "/rules";

/// Root-level UI metadata when there is no account/draft/message context.
pub fn root_ui() -> Value {
    let origin = dashboard_origin();
    let mut ui = json!({
        "dashboard_url": origin.base_url,
        "dashboard_path": "/",
    });
    attach_dashboard_origin_metadata(&mut ui, &origin);
    ui
}

/// UI metadata anchored at the agent cockpit.
///
/// `account_id` is accepted so every call site stays account-aware, but the
/// cockpit itself is a single global route: the account is selected inside the
/// page, not in the URL.
pub fn account_ui(_account_id: &str) -> Value {
    let origin = dashboard_origin();
    let mut ui = json!({
        "dashboard_url": origin.base_url,
        "dashboard_path": COCKPIT_PATH,
        "cockpit_url": join(&origin.base_url, COCKPIT_PATH),
    });
    attach_dashboard_origin_metadata(&mut ui, &origin);
    ui
}

/// UI metadata for a specific draft: `review_url` points at the draft
/// approval surface, which *is* account-scoped in the SPA.
pub fn draft_ui(account_id: &str, draft_id: &str) -> Value {
    let origin = dashboard_origin();
    let acct = encode_segment(account_id);
    let draft = encode_segment(draft_id);
    let draft_path = format!("/accounts/{acct}/drafts/{draft}");
    let mut ui = json!({
        "dashboard_url": origin.base_url,
        "dashboard_path": draft_path.clone(),
        "cockpit_url": join(&origin.base_url, COCKPIT_PATH),
        "review_url": join(&origin.base_url, &draft_path),
    });
    attach_dashboard_origin_metadata(&mut ui, &origin);
    ui
}

/// UI metadata for rule-related responses; `rules_url` is the rules panel.
pub fn rules_ui(_account_id: &str) -> Value {
    let origin = dashboard_origin();
    let mut ui = json!({
        "dashboard_url": origin.base_url,
        "dashboard_path": RULES_PATH,
        "cockpit_url": join(&origin.base_url, COCKPIT_PATH),
        "rules_url": join(&origin.base_url, RULES_PATH),
    });
    attach_dashboard_origin_metadata(&mut ui, &origin);
    ui
}

/// UI metadata for a specific message; `message_url` is the canonical reader
/// route and carries the folder as a query parameter, because IMAP UIDs are
/// mailbox-scoped and the dashboard must re-resolve the UID in the right one.
pub fn message_ui(account_id: &str, uid: u32, folder: &str) -> Value {
    let origin = dashboard_origin();
    let acct = encode_segment(account_id);
    let folder_enc = encode_segment(folder);
    let msg_path = format!("/mail/unified/{acct}/{uid}?folder={folder_enc}");
    let mut ui = json!({
        "dashboard_url": origin.base_url,
        "dashboard_path": msg_path.clone(),
        "cockpit_url": join(&origin.base_url, COCKPIT_PATH),
        "message_url": join(&origin.base_url, &msg_path),
    });
    attach_dashboard_origin_metadata(&mut ui, &origin);
    ui
}

/// UI metadata for a message UID that may name an editable draft.
///
/// A UID in the Drafts folder points at a message the reader route can only
/// *display*: `/mail/unified/...` has no recipient fields and no Send. When the
/// folder classifies as drafts and a local draft row carries that UID, the link
/// must resolve to the draft review composer instead — the one surface that can
/// edit and send it.
///
/// `message_url` is set to the same review URL as `review_url` because callers
/// already read `message_url` off message payloads; leaving it on the reader
/// would hand out the dead-end link next to the working one. Every other folder
/// keeps today's [`message_ui`] shape.
pub fn message_or_draft_ui(db: &Database, account_id: &str, uid: u32, folder: &str) -> Value {
    match local_draft_for_imap_uid(db, account_id, uid, folder) {
        Some(draft_id) => {
            let mut ui = draft_ui(account_id, &draft_id);
            let review_url = ui["review_url"].clone();
            if let Value::Object(map) = &mut ui {
                map.insert("message_url".to_string(), review_url);
            }
            ui
        }
        None => message_ui(account_id, uid, folder),
    }
}

/// The local draft id behind an IMAP Drafts-folder UID, when there is one.
///
/// A lookup failure is reported and treated as "no local draft": the reader URL
/// is still a correct link for the UID, so a degraded database must not take
/// down the whole command that was only annotating a response.
fn local_draft_for_imap_uid(
    db: &Database,
    account_id: &str,
    uid: u32,
    folder: &str,
) -> Option<String> {
    if provider::classify_folder(folder) != Some("drafts") {
        return None;
    }
    match db.get_draft_by_imap_uid(account_id, uid) {
        Ok(draft) => draft.map(|d| d.id),
        Err(e) => {
            warn!("draft lookup for {folder} uid {uid} failed, linking to the reader instead: {e}");
            None
        }
    }
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
    fn configured_dashboard_origins_do_not_affect_agent_ui_urls() {
        let _guard = isolated_dashboard_config("configured-origin-ignored");
        let path = crate::commands::config::test_config_file_path().unwrap();
        std::fs::write(
            path,
            r#"{"dashboard":{"base_url":"https://stale.example.test"}}"#,
        )
        .unwrap();
        assert_eq!(dashboard_base(), "http://localhost:3141");
    }

    #[test]
    fn tailscale_serve_status_discovers_the_active_loopback_dashboard_route() {
        let status = r#"{
            "TCP": {"443": {"HTTPS": true}},
            "Web": {"14inches.tail87a011.ts.net:443": {
                "Handlers": {"/": {"Proxy": "http://127.0.0.1:3141"}}
            }}
        }"#;
        assert_eq!(
            parse_tailscale_serve_status(status).as_deref(),
            Some("https://14inches.tail87a011.ts.net")
        );
    }

    #[test]
    fn tailscale_serve_status_rejects_wrong_proxy_or_missing_https() {
        let wrong_proxy = r#"{
            "TCP": {"443": {"HTTPS": true}},
            "Web": {"node.tailnet.ts.net:443": {
                "Handlers": {"/": {"Proxy": "http://127.0.0.1:3142"}}
            }}
        }"#;
        let no_https = r#"{
            "TCP": {"443": {"HTTPS": false}},
            "Web": {"node.tailnet.ts.net:443": {
                "Handlers": {"/": {"Proxy": "http://127.0.0.1:3141"}}
            }}
        }"#;
        assert_eq!(parse_tailscale_serve_status(wrong_proxy), None);
        assert_eq!(parse_tailscale_serve_status(no_https), None);
    }

    #[test]
    fn tailscale_serve_status_fails_closed_for_malformed_or_ambiguous_routes() {
        let ambiguous = r#"{
            "TCP": {"443": {"HTTPS": true}},
            "Web": {
                "one.tailnet.ts.net:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:3141"}}},
                "two.tailnet.ts.net:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:3141/"}}}
            }
        }"#;
        assert_eq!(parse_tailscale_serve_status("not json"), None);
        assert_eq!(parse_tailscale_serve_status(ambiguous), None);
    }

    #[test]
    fn tailscale_serve_status_normalizes_host_and_port() {
        let status = r#"{
            "TCP": {"443": {"HTTPS": true}, "8443": {"HTTPS": true}},
            "Web": {
                "14INCHES.tail87a011.ts.net.:0443": {
                    "Handlers": {"/": {"Proxy": "http://127.0.0.1:3141/"}}
                },
                "ignored.tailnet.ts.net:8443": {
                    "Handlers": {"/": {"Proxy": "http://127.0.0.1:3141"}}
                }
            }
        }"#;
        assert_eq!(
            parse_tailscale_serve_status(status).as_deref(),
            Some("https://14inches.tail87a011.ts.net")
        );
    }

    #[test]
    fn matching_tailscale_serve_and_self_hostname_has_no_warning() {
        let serve = r#"{
            "TCP": {"443": {"HTTPS": true}},
            "Web": {"14inches.tail87a011.ts.net:443": {
                "Handlers": {"/": {"Proxy": "http://127.0.0.1:3141"}}
            }}
        }"#;
        let node = r#"{"Self":{"DNSName":"14INCHES.tail87a011.ts.net."}}"#;

        let origin = resolve_dashboard_origin(Some(serve), Some(node));
        assert_eq!(origin.base_url, "https://14inches.tail87a011.ts.net");
        assert_eq!(origin.dashboard_origin_source, "tailscale_serve");
        assert_eq!(origin.dashboard_origin_warning, None);
    }

    #[test]
    fn different_tailscale_serve_and_self_hostname_keeps_serve_with_safe_warning() {
        let serve = r#"{
            "TCP": {"443": {"HTTPS": true}},
            "Web": {"14inches.tail87a011.ts.net:443": {
                "Handlers": {"/": {"Proxy": "http://127.0.0.1:3141"}}
            }}
        }"#;
        let node = r#"{"Self":{"DNSName":"rey2026.tail87a011.ts.net."}}"#;

        let origin = resolve_dashboard_origin(Some(serve), Some(node));
        assert_eq!(origin.base_url, "https://14inches.tail87a011.ts.net");
        assert_eq!(origin.dashboard_origin_source, "tailscale_serve");
        let warning = origin.dashboard_origin_warning.as_deref().unwrap();
        assert!(warning.contains("`14inches`"));
        assert!(warning.contains("`rey2026`"));
        assert!(!warning.contains("tail87a011"));
    }

    #[test]
    fn online_node_without_envelope_serve_route_falls_back_with_safe_warning() {
        let serve = r#"{
            "TCP": {"443": {"HTTPS": true}},
            "Web": {"rey2026.tail87a011.ts.net:443": {
                "Handlers": {"/": {"Proxy": "http://127.0.0.1:3142"}}
            }}
        }"#;
        let node = r#"{"Self":{"DNSName":"rey2026.tail87a011.ts.net."}}"#;

        let origin = resolve_dashboard_origin(Some(serve), Some(node));
        assert_eq!(origin.base_url, DEFAULT_DASHBOARD_BASE);
        assert_eq!(origin.dashboard_origin_source, "localhost_fallback");
        let warning = origin.dashboard_origin_warning.as_deref().unwrap();
        assert!(warning.contains("online but is not serving Envelope"));
        assert!(warning.contains("`rey2026`"));
        assert!(!warning.contains("tail87a011"));
    }

    #[test]
    fn malformed_or_unavailable_tailscale_status_falls_back() {
        let origin = resolve_dashboard_origin(Some("not json"), None);
        assert_eq!(origin.base_url, DEFAULT_DASHBOARD_BASE);
        assert_eq!(origin.dashboard_origin_source, "localhost_fallback");
        assert!(
            origin
                .dashboard_origin_warning
                .as_deref()
                .unwrap()
                .contains("unavailable or returned malformed status")
        );
        assert_eq!(
            parse_tailscale_self_dns_name(r#"{"Self":{"DNSName":"bad host"}}"#),
            None
        );
    }

    #[test]
    fn every_ui_object_includes_origin_source_and_omits_empty_warning() {
        let ui_objects = [
            root_ui(),
            account_ui("acct-1"),
            draft_ui("acct-1", "draft-1"),
            rules_ui("acct-1"),
            message_ui("acct-1", 42, "INBOX"),
        ];
        for ui in ui_objects {
            assert_eq!(ui["dashboard_origin_source"], "localhost_fallback");
            assert!(ui.get("dashboard_origin_warning").is_none());
        }
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
    fn account_ui_points_at_the_global_cockpit_route() {
        let _guard = isolated_dashboard_config("account-default");
        let ui = account_ui("acct-1");
        assert_eq!(ui["dashboard_url"], "http://localhost:3141");
        assert_eq!(ui["dashboard_path"], "/cockpit");
        assert_eq!(ui["cockpit_url"], "http://localhost:3141/cockpit");
    }

    #[test]
    fn account_ui_ignores_configured_base_url_for_cockpit_url() {
        let _guard = isolated_dashboard_config("account-configured-base");
        let path = crate::commands::config::test_config_file_path().unwrap();
        std::fs::write(
            path,
            r#"{"dashboard":{"base_url":"https://dash.example.test/envelope"}}"#,
        )
        .unwrap();

        let ui = account_ui("acct/one");
        assert_eq!(ui["dashboard_url"], "http://localhost:3141");
        assert_eq!(ui["dashboard_path"], "/cockpit");
        assert_eq!(ui["cockpit_url"], "http://localhost:3141/cockpit");
    }

    #[test]
    fn draft_ui_keeps_review_url_and_uses_global_cockpit() {
        let _guard = isolated_dashboard_config("draft-default");
        let ui = draft_ui("acct-1", "draft-abc");
        assert_eq!(ui["dashboard_path"], "/accounts/acct-1/drafts/draft-abc");
        assert_eq!(
            ui["review_url"],
            "http://localhost:3141/accounts/acct-1/drafts/draft-abc"
        );
        assert_eq!(ui["cockpit_url"], "http://localhost:3141/cockpit");
    }

    #[test]
    fn rules_ui_points_at_the_global_rules_route() {
        let _guard = isolated_dashboard_config("rules-default");
        let ui = rules_ui("acct-1");
        assert_eq!(ui["dashboard_path"], "/rules");
        assert_eq!(ui["rules_url"], "http://localhost:3141/rules");
        assert_eq!(ui["cockpit_url"], "http://localhost:3141/cockpit");
    }

    #[test]
    fn message_ui_escapes_folder_and_keeps_uid() {
        let _guard = isolated_dashboard_config("message-default");
        let ui = message_ui("acct-1", 42, "Sent Items");
        assert_eq!(
            ui["dashboard_path"],
            "/mail/unified/acct-1/42?folder=Sent%20Items"
        );
        assert_eq!(
            ui["message_url"],
            "http://localhost:3141/mail/unified/acct-1/42?folder=Sent%20Items"
        );
    }

    #[test]
    fn message_ui_ignores_configured_base_url_and_preserves_query_encoding() {
        let _guard = isolated_dashboard_config("message-configured-base");
        let path = crate::commands::config::test_config_file_path().unwrap();
        std::fs::write(
            path,
            r#"{"dashboard":{"base_url":"https://dash.example.test/envelope"}}"#,
        )
        .unwrap();

        let ui = message_ui("acct/one", 42, "Sent/Items & Stuff");
        assert_eq!(
            ui["message_url"],
            "http://localhost:3141/mail/unified/acct%2Fone/42?folder=Sent%2FItems%20%26%20Stuff"
        );
        assert_eq!(ui["cockpit_url"], "http://localhost:3141/cockpit");
    }

    /// The exact link shape reproduced against installed 1.0.10: a UUID account
    /// and a Gmail folder carrying both `[` `]` and a space. The old
    /// `/accounts/{id}/messages/{uid}` shape had no client route and rendered the
    /// SvelteKit 404 page.
    #[test]
    fn message_ui_emits_the_canonical_reader_route_for_gmail_sent_mail() {
        let _guard = isolated_dashboard_config("message-gmail-sent");
        let ui = message_ui(
            "109c5747-8498-4614-945a-837462ae0aaf",
            33281,
            "[Gmail]/Sent Mail",
        );
        assert_eq!(
            ui["message_url"],
            "http://localhost:3141/mail/unified/109c5747-8498-4614-945a-837462ae0aaf/33281\
             ?folder=%5BGmail%5D%2FSent%20Mail"
        );
    }

    /// In-memory database holding one account with a single local draft that
    /// has been synced to the given Drafts UID.
    fn db_with_synced_draft(account_id: &str, imap_uid: u32) -> (Database, String) {
        let db = Database::open_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, username, domain, smtp_host, smtp_port,
                 imap_host, imap_port, encrypted_password)
                 VALUES (?1, 'Spain Expat', 'editor@spainexpat.com', 'spainexpat.com',
                         'smtp.spainexpat.com', 587, 'imap.spainexpat.com', 993, 'encrypted')",
                [account_id],
            )
            .unwrap();
        let draft = db
            .create_draft(
                account_id,
                "tyler@example.com",
                Some("Review this reply"),
                Some("Looks ready to send."),
                None,
                None,
                None,
                None,
                Some("agent"),
            )
            .unwrap();
        db.update_draft_imap_uid(&draft.id, imap_uid).unwrap();
        (db, draft.id)
    }

    /// The bug: `envelope read --folder Drafts --json` handed back the
    /// `/mail/unified/...` reader, which cannot edit or send. A Drafts UID that
    /// resolves to a local draft must link to the review composer instead, on
    /// both `review_url` and `message_url`.
    #[test]
    fn message_or_draft_ui_resolves_a_synced_draft_to_the_review_composer() {
        let _guard = isolated_dashboard_config("draft-by-imap-uid");
        let (db, draft_id) = db_with_synced_draft("acct-1", 38311);

        for folder in ["Drafts", "[Gmail]/Drafts", "INBOX.Drafts", "drafts"] {
            let ui = message_or_draft_ui(&db, "acct-1", 38311, folder);
            let expected = format!("http://localhost:3141/accounts/acct-1/drafts/{draft_id}");
            assert_eq!(
                ui["dashboard_path"],
                format!("/accounts/acct-1/drafts/{draft_id}"),
                "{folder} dashboard_path"
            );
            assert_eq!(ui["review_url"], expected, "{folder} review_url");
            assert_eq!(ui["message_url"], expected, "{folder} message_url");
        }
    }

    /// Non-draft folders are untouched: the reader is the right surface there,
    /// and a Drafts UID that has no local draft row has nothing better to offer.
    #[test]
    fn message_or_draft_ui_keeps_the_reader_route_for_everything_else() {
        let _guard = isolated_dashboard_config("draft-by-imap-uid-miss");
        let (db, _) = db_with_synced_draft("acct-1", 38311);

        let inbox = message_or_draft_ui(&db, "acct-1", 57, "INBOX");
        assert_eq!(
            inbox["dashboard_path"],
            "/mail/unified/acct-1/57?folder=INBOX"
        );
        assert_eq!(
            inbox["message_url"],
            "http://localhost:3141/mail/unified/acct-1/57?folder=INBOX"
        );
        assert!(inbox.get("review_url").is_none());

        let sent = message_or_draft_ui(&db, "acct-1", 38311, "[Gmail]/Sent Mail");
        assert_eq!(
            sent["message_url"],
            "http://localhost:3141/mail/unified/acct-1/38311?folder=%5BGmail%5D%2FSent%20Mail"
        );

        // Drafts folder, but no local draft carries this uid.
        let orphan = message_or_draft_ui(&db, "acct-1", 999, "Drafts");
        assert_eq!(
            orphan["message_url"],
            "http://localhost:3141/mail/unified/acct-1/999?folder=Drafts"
        );

        // Right uid, wrong account — drafts must never leak across accounts.
        let other = message_or_draft_ui(&db, "acct-2", 38311, "Drafts");
        assert_eq!(
            other["message_url"],
            "http://localhost:3141/mail/unified/acct-2/38311?folder=Drafts"
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
