// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Persistent CLI configuration.
//!
//! This intentionally uses a small JSON file in Envelope's existing app-data
//! directory instead of mailbox storage. UI metadata must remain available
//! without opening or migrating the mail database.

use crate::ConfigCmd;
use anyhow::{Context, Result, bail};
use envelope_email_store::app_data_dir;
use serde_json::{Map, Value, json};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const DASHBOARD_BASE_URL_KEY: &str = "dashboard.base_url";

/// Bearer token gating the dashboard REST API when exposed beyond loopback.
pub const DASHBOARD_AUTH_TOKEN_KEY: &str = "dashboard.auth_token";
pub const ENV_DASHBOARD_TOKEN: &str = "ENVELOPE_DASHBOARD_TOKEN";
/// Comma-separated Tailscale identity allowlist (`Tailscale-User-Login` values).
pub const DASHBOARD_TAILSCALE_ALLOW_KEY: &str = "dashboard.tailscale_allow";
pub const ENV_DASHBOARD_TAILSCALE_ALLOW: &str = "ENVELOPE_DASHBOARD_TAILSCALE_ALLOW";

const CONFIG_FILE_NAME: &str = "config.json";

fn cmd_key(cmd: &ConfigCmd) -> &str {
    match cmd {
        ConfigCmd::Get { key } | ConfigCmd::Set { key, .. } | ConfigCmd::Unset { key } => key,
    }
}

pub fn run(cmd: ConfigCmd, json_output: bool) -> Result<()> {
    // The auth-token and tailscale-allow keys share generic string handling and
    // must never echo the token value.
    let key = cmd_key(&cmd).to_string();
    if key == DASHBOARD_AUTH_TOKEN_KEY || key == DASHBOARD_TAILSCALE_ALLOW_KEY {
        return run_generic_dashboard_field(cmd, &key, json_output);
    }
    match cmd {
        ConfigCmd::Get { key } => {
            require_supported_key(&key)?;
            let stored_value = persistent_dashboard_base_url()?;
            if json_output {
                let value = super::ui::with_ui(
                    &json!({
                        "key": DASHBOARD_BASE_URL_KEY,
                        "value": stored_value,
                        "agent_ui_origin": "discovered from active tailscale serve or http://localhost:3141; dashboard_origin_source explains the selected origin; dashboard.base_url does not affect agent UI links",
                        "config_path": display_config_path(),
                    }),
                    super::ui::root_ui(),
                );
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if let Some(value) = stored_value {
                println!("{DASHBOARD_BASE_URL_KEY}={value}");
                println!(
                    "Note: {DASHBOARD_BASE_URL_KEY} is retained for compatibility and does not affect agent UI links."
                );
            } else {
                println!("{DASHBOARD_BASE_URL_KEY} is not set");
                println!(
                    "Note: {DASHBOARD_BASE_URL_KEY} does not affect agent UI links; their origin is discovered from active tailscale serve or falls back to http://localhost:3141."
                );
            }
            Ok(())
        }
        ConfigCmd::Set { key, value } => {
            require_supported_key(&key)?;
            let normalized = normalize_dashboard_base(&value).ok_or_else(|| {
                anyhow::anyhow!(
                    "{DASHBOARD_BASE_URL_KEY} cannot be empty; use `envelope config unset {DASHBOARD_BASE_URL_KEY}`"
                )
            })?;
            set_persistent_dashboard_base_url(&normalized)?;
            if json_output {
                let value = super::ui::with_ui(
                    &json!({
                        "status": "set",
                        "key": DASHBOARD_BASE_URL_KEY,
                        "value": normalized,
                        "agent_ui_origin": "discovered from active tailscale serve or http://localhost:3141; dashboard_origin_source explains the selected origin; dashboard.base_url does not affect agent UI links",
                        "config_path": display_config_path(),
                    }),
                    super::ui::root_ui(),
                );
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("Set {DASHBOARD_BASE_URL_KEY}={normalized}");
                println!(
                    "Note: this compatibility setting does not affect agent UI links; their origin is discovered from active tailscale serve or falls back to http://localhost:3141."
                );
            }
            Ok(())
        }
        ConfigCmd::Unset { key } => {
            require_supported_key(&key)?;
            unset_persistent_dashboard_base_url()?;
            if json_output {
                let value = super::ui::with_ui(
                    &json!({
                        "status": "unset",
                        "key": DASHBOARD_BASE_URL_KEY,
                        "value": Value::Null,
                        "agent_ui_origin": "discovered from active tailscale serve or http://localhost:3141; dashboard_origin_source explains the selected origin; dashboard.base_url does not affect agent UI links",
                        "config_path": display_config_path(),
                    }),
                    super::ui::root_ui(),
                );
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("Unset {DASHBOARD_BASE_URL_KEY}");
                println!(
                    "Note: {DASHBOARD_BASE_URL_KEY} does not affect agent UI links; their origin is discovered from active tailscale serve or falls back to http://localhost:3141."
                );
            }
            Ok(())
        }
    }
}

pub fn persistent_dashboard_base_url() -> Result<Option<String>> {
    persistent_dashboard_base_url_from(&config_file_path())
}

fn set_persistent_dashboard_base_url(value: &str) -> Result<()> {
    let path = config_file_path();
    let mut config = read_config_value(&path)?;
    set_dashboard_base_url_value(&mut config, value)?;
    write_config_value(&path, &config)
}

fn unset_persistent_dashboard_base_url() -> Result<()> {
    let path = config_file_path();
    let mut config = read_config_value(&path)?;
    unset_dashboard_base_url_value(&mut config)?;
    write_config_value(&path, &config)
}

fn persistent_dashboard_base_url_from(path: &Path) -> Result<Option<String>> {
    let config = read_config_value(path)?;
    match config.pointer("/dashboard/base_url") {
        Some(Value::String(value)) => Ok(normalize_dashboard_base(value)),
        Some(Value::Null) | None => Ok(None),
        Some(_) => bail!("{DASHBOARD_BASE_URL_KEY} must be a string"),
    }
}

fn normalize_dashboard_base(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.trim_end_matches('/').to_string())
}

fn require_supported_key(key: &str) -> Result<()> {
    match key {
        DASHBOARD_BASE_URL_KEY | DASHBOARD_AUTH_TOKEN_KEY | DASHBOARD_TAILSCALE_ALLOW_KEY => Ok(()),
        _ => bail!(
            "unknown config key `{key}`; supported keys: {DASHBOARD_BASE_URL_KEY}, \
             {DASHBOARD_AUTH_TOKEN_KEY}, {DASHBOARD_TAILSCALE_ALLOW_KEY}"
        ),
    }
}

/// True for keys whose stored value is a secret and must never be printed.
fn is_secret_key(key: &str) -> bool {
    key == DASHBOARD_AUTH_TOKEN_KEY
}

/// The JSON pointer for a `dashboard.<field>` key, e.g. `/dashboard/auth_token`.
fn dashboard_pointer(key: &str) -> String {
    format!("/dashboard/{}", key.trim_start_matches("dashboard."))
}

/// Generic get/set/unset for simple `dashboard.<field>` string config keys.
/// Secret keys report presence only — the value is never echoed.
fn run_generic_dashboard_field(cmd: ConfigCmd, key: &str, json_output: bool) -> Result<()> {
    let pointer = dashboard_pointer(key);
    let secret = is_secret_key(key);
    match cmd {
        ConfigCmd::Get { .. } => {
            let stored = read_dashboard_string(&pointer)?;
            let present = stored.is_some();
            if json_output {
                let mut obj = json!({
                    "key": key,
                    "configured": present,
                    "config_path": display_config_path(),
                });
                if !secret {
                    obj["value"] = stored.clone().map(Value::String).unwrap_or(Value::Null);
                }
                let value = super::ui::with_ui(&obj, super::ui::root_ui());
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if secret {
                println!("{key}={}", if present { "<configured>" } else { "not set" });
            } else if let Some(value) = stored {
                println!("{key}={value}");
            } else {
                println!("{key} is not set");
            }
            Ok(())
        }
        ConfigCmd::Set { value, .. } => {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                bail!("{key} cannot be empty; use `envelope config unset {key}`");
            }
            write_dashboard_string(&pointer, &trimmed)?;
            if json_output {
                let obj = json!({
                    "status": "set",
                    "key": key,
                    "configured": true,
                    "config_path": display_config_path(),
                });
                let value = super::ui::with_ui(&obj, super::ui::root_ui());
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if secret {
                println!("Set {key} (value hidden)");
            } else {
                println!("Set {key}={trimmed}");
            }
            Ok(())
        }
        ConfigCmd::Unset { .. } => {
            clear_dashboard_string(&pointer)?;
            if json_output {
                let obj = json!({
                    "status": "unset",
                    "key": key,
                    "configured": false,
                    "config_path": display_config_path(),
                });
                let value = super::ui::with_ui(&obj, super::ui::root_ui());
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("Unset {key}");
            }
            Ok(())
        }
    }
}

fn read_dashboard_string(pointer: &str) -> Result<Option<String>> {
    let config = read_config_value(&config_file_path())?;
    match config.pointer(pointer) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        _ => Ok(None),
    }
}

fn write_dashboard_string(pointer: &str, value: &str) -> Result<()> {
    let path = config_file_path();
    let mut config = read_config_value(&path)?;
    let field = pointer.trim_start_matches("/dashboard/");
    let root = config_object(&mut config)?;
    let dashboard = root
        .entry("dashboard".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    config_object(dashboard)?.insert(field.to_string(), Value::String(value.to_string()));
    write_config_value(&path, &config)
}

fn clear_dashboard_string(pointer: &str) -> Result<()> {
    let path = config_file_path();
    let mut config = read_config_value(&path)?;
    let field = pointer.trim_start_matches("/dashboard/");
    let root = config_object(&mut config)?;
    if let Some(dashboard) = root.get_mut("dashboard") {
        let dashboard = config_object(dashboard)?;
        dashboard.remove(field);
        if dashboard.is_empty() {
            root.remove("dashboard");
        }
    }
    write_config_value(&path, &config)
}

/// Resolve the dashboard bearer token: env `ENVELOPE_DASHBOARD_TOKEN` wins, then
/// the persisted `dashboard.auth_token`. Returns `None` when unset.
pub fn resolved_dashboard_auth_token() -> Option<String> {
    if let Ok(value) = std::env::var(ENV_DASHBOARD_TOKEN) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    read_dashboard_string(&dashboard_pointer(DASHBOARD_AUTH_TOKEN_KEY))
        .ok()
        .flatten()
}

/// Resolve the Tailscale identity allowlist: env
/// `ENVELOPE_DASHBOARD_TAILSCALE_ALLOW` wins, then persisted
/// `dashboard.tailscale_allow`. Returns the parsed entries (possibly empty).
pub fn resolved_dashboard_tailscale_allow() -> Vec<String> {
    let raw = std::env::var(ENV_DASHBOARD_TAILSCALE_ALLOW)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            read_dashboard_string(&dashboard_pointer(DASHBOARD_TAILSCALE_ALLOW_KEY))
                .ok()
                .flatten()
        });
    match raw {
        Some(value) => value
            .split([',', '\n', ' ', '\t'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        None => Vec::new(),
    }
}

fn config_file_path() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = test_config_file_path() {
        return path;
    }

    app_data_dir().join(CONFIG_FILE_NAME)
}

fn display_config_path() -> String {
    config_file_path().display().to_string()
}

fn read_config_value(path: &Path) -> Result<Value> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(json!({})),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };

    if contents.trim().is_empty() {
        return Ok(json!({}));
    }

    let value: Value = serde_json::from_str(&contents)
        .with_context(|| format!("parse JSON config {}", path.display()))?;
    if !value.is_object() {
        bail!("config file {} must contain a JSON object", path.display());
    }
    Ok(value)
}

fn write_config_value(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let mut contents = serde_json::to_string_pretty(value).context("serialize config")?;
    contents.push('\n');
    fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("set permissions on {}", path.display()))?;
    }

    Ok(())
}

fn set_dashboard_base_url_value(config: &mut Value, value: &str) -> Result<()> {
    let root = config_object(config)?;
    let dashboard = root
        .entry("dashboard".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let dashboard = config_object(dashboard)?;
    dashboard.insert("base_url".to_string(), Value::String(value.to_string()));
    Ok(())
}

fn unset_dashboard_base_url_value(config: &mut Value) -> Result<()> {
    let root = config_object(config)?;
    if let Some(dashboard) = root.get_mut("dashboard") {
        let dashboard = config_object(dashboard)?;
        dashboard.remove("base_url");
        if dashboard.is_empty() {
            root.remove("dashboard");
        }
    }
    Ok(())
}

fn config_object(value: &mut Value) -> Result<&mut Map<String, Value>> {
    match value {
        Value::Object(map) => Ok(map),
        _ => bail!("config value must be a JSON object"),
    }
}

#[cfg(test)]
mod test_support {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    thread_local! {
        static TEST_CONFIG_FILE_PATH: RefCell<Option<PathBuf>> = RefCell::new(None);
    }

    static TEST_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    pub(crate) struct DashboardConfigTestGuard {
        _guard: MutexGuard<'static, ()>,
    }

    impl Drop for DashboardConfigTestGuard {
        fn drop(&mut self) {
            TEST_CONFIG_FILE_PATH.with(|path| *path.borrow_mut() = None);
        }
    }

    pub(crate) fn isolated_dashboard_config(path: PathBuf) -> DashboardConfigTestGuard {
        let guard = TEST_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("dashboard config test env lock poisoned");
        TEST_CONFIG_FILE_PATH.with(|slot| *slot.borrow_mut() = Some(path));
        DashboardConfigTestGuard { _guard: guard }
    }

    pub(crate) fn test_config_file_path() -> Option<PathBuf> {
        TEST_CONFIG_FILE_PATH.with(|path| path.borrow().clone())
    }
}

#[cfg(test)]
pub(crate) use test_support::{
    DashboardConfigTestGuard, isolated_dashboard_config, test_config_file_path,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "envelope-config-test-{}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn persisted_dashboard_base_url_is_retained_as_compatibility_data() {
        let path = test_config_path("compatibility-data");
        let _ = fs::remove_file(&path);
        let _guard = isolated_dashboard_config(path.clone());
        write_config_value(
            &path,
            &json!({"dashboard": {"base_url": "https://config.example/"}}),
        )
        .unwrap();

        assert_eq!(
            persistent_dashboard_base_url().unwrap(),
            Some("https://config.example".to_string())
        );
    }

    #[test]
    fn persistent_dashboard_base_url_preserves_unknown_keys() {
        let path = test_config_path("preserve-unknown");
        let _ = fs::remove_file(&path);
        let _guard = isolated_dashboard_config(path.clone());
        write_config_value(&path, &json!({"other": {"enabled": true}})).unwrap();

        set_persistent_dashboard_base_url("https://dash.example/").unwrap();
        assert_eq!(
            persistent_dashboard_base_url().unwrap(),
            Some("https://dash.example".to_string())
        );
        let config = read_config_value(&path).unwrap();
        assert_eq!(config["other"]["enabled"], true);

        unset_persistent_dashboard_base_url().unwrap();
        let config = read_config_value(&path).unwrap();
        assert_eq!(config["other"]["enabled"], true);
        assert!(config.pointer("/dashboard/base_url").is_none());
    }

    #[test]
    fn rejects_non_string_dashboard_base_url_config() {
        let path = test_config_path("non-string");
        let _ = fs::remove_file(&path);
        let _guard = isolated_dashboard_config(path.clone());
        write_config_value(&path, &json!({"dashboard": {"base_url": 42}})).unwrap();

        let err = persistent_dashboard_base_url().unwrap_err().to_string();
        assert!(err.contains("dashboard.base_url must be a string"));
    }

    #[test]
    fn auth_token_round_trips_and_preserves_base_url() {
        let path = test_config_path("auth-token-roundtrip");
        let _ = fs::remove_file(&path);
        let _guard = isolated_dashboard_config(path.clone());
        write_config_value(
            &path,
            &json!({"dashboard": {"base_url": "https://dash.example"}}),
        )
        .unwrap();

        let ptr = dashboard_pointer(DASHBOARD_AUTH_TOKEN_KEY);
        assert_eq!(ptr, "/dashboard/auth_token");
        write_dashboard_string(&ptr, "s3cret").unwrap();

        // Token stored, base_url untouched.
        assert_eq!(read_dashboard_string(&ptr).unwrap(), Some("s3cret".into()));
        assert_eq!(
            persistent_dashboard_base_url().unwrap(),
            Some("https://dash.example".to_string())
        );

        // Clearing the token leaves base_url intact.
        clear_dashboard_string(&ptr).unwrap();
        assert_eq!(read_dashboard_string(&ptr).unwrap(), None);
        assert_eq!(
            persistent_dashboard_base_url().unwrap(),
            Some("https://dash.example".to_string())
        );
    }

    #[test]
    fn tailscale_allow_config_parses_into_entries() {
        let path = test_config_path("tailscale-allow-parse");
        let _ = fs::remove_file(&path);
        let _guard = isolated_dashboard_config(path.clone());
        // Env must be unset for the config value to win; the guard clears the
        // base-url envs but not ours, so set explicitly for the assertion.
        unsafe { std::env::remove_var(ENV_DASHBOARD_TAILSCALE_ALLOW) };
        write_dashboard_string(
            &dashboard_pointer(DASHBOARD_TAILSCALE_ALLOW_KEY),
            "skippy@tail.ts.net, tyler@tail.ts.net",
        )
        .unwrap();

        let allow = resolved_dashboard_tailscale_allow();
        assert_eq!(allow, vec!["skippy@tail.ts.net", "tyler@tail.ts.net"]);
    }

    #[test]
    fn is_secret_key_only_true_for_auth_token() {
        assert!(is_secret_key(DASHBOARD_AUTH_TOKEN_KEY));
        assert!(!is_secret_key(DASHBOARD_TAILSCALE_ALLOW_KEY));
        assert!(!is_secret_key(DASHBOARD_BASE_URL_KEY));
    }
}
