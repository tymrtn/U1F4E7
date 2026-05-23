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
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const DASHBOARD_BASE_URL_KEY: &str = "dashboard.base_url";
pub const ENV_DASHBOARD_BASE_URL: &str = "ENVELOPE_DASHBOARD_BASE_URL";
pub const ENV_DASHBOARD_URL_ALIAS: &str = "ENVELOPE_DASHBOARD_URL";

const CONFIG_FILE_NAME: &str = "config.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedDashboardBaseUrl {
    pub value: String,
    pub source: &'static str,
}

pub fn run(cmd: ConfigCmd, json_output: bool) -> Result<()> {
    match cmd {
        ConfigCmd::Get { key } => {
            require_supported_key(&key)?;
            let stored_value = persistent_dashboard_base_url()?;
            let effective = resolved_dashboard_base_url();
            if json_output {
                let value = super::ui::with_ui(
                    &json!({
                        "key": DASHBOARD_BASE_URL_KEY,
                        "value": stored_value,
                        "effective_value": effective.value,
                        "source": effective.source,
                        "config_path": display_config_path(),
                    }),
                    super::ui::root_ui(),
                );
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if let Some(value) = stored_value {
                println!("{DASHBOARD_BASE_URL_KEY}={value}");
                println!("effective {DASHBOARD_BASE_URL_KEY}={}", effective.value);
                println!("source={}", effective.source);
            } else {
                println!("{DASHBOARD_BASE_URL_KEY} is not set");
                println!("effective {DASHBOARD_BASE_URL_KEY}={}", effective.value);
                println!("source={}", effective.source);
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
            let effective = resolved_dashboard_base_url();
            if json_output {
                let value = super::ui::with_ui(
                    &json!({
                        "status": "set",
                        "key": DASHBOARD_BASE_URL_KEY,
                        "value": normalized,
                        "effective_value": effective.value,
                        "source": effective.source,
                        "config_path": display_config_path(),
                    }),
                    super::ui::root_ui(),
                );
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("Set {DASHBOARD_BASE_URL_KEY}={normalized}");
                if effective.source != "config" {
                    println!(
                        "Effective value is still {} from {}",
                        effective.value, effective.source
                    );
                }
            }
            Ok(())
        }
        ConfigCmd::Unset { key } => {
            require_supported_key(&key)?;
            unset_persistent_dashboard_base_url()?;
            let effective = resolved_dashboard_base_url();
            if json_output {
                let value = super::ui::with_ui(
                    &json!({
                        "status": "unset",
                        "key": DASHBOARD_BASE_URL_KEY,
                        "value": Value::Null,
                        "effective_value": effective.value,
                        "source": effective.source,
                        "config_path": display_config_path(),
                    }),
                    super::ui::root_ui(),
                );
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("Unset {DASHBOARD_BASE_URL_KEY}");
                println!("effective {DASHBOARD_BASE_URL_KEY}={}", effective.value);
                println!("source={}", effective.source);
            }
            Ok(())
        }
    }
}

pub fn resolved_dashboard_base_url() -> ResolvedDashboardBaseUrl {
    if let Some(value) = dashboard_base_url_from_env(ENV_DASHBOARD_BASE_URL) {
        return ResolvedDashboardBaseUrl {
            value,
            source: "env:ENVELOPE_DASHBOARD_BASE_URL",
        };
    }

    if let Some(value) = dashboard_base_url_from_env(ENV_DASHBOARD_URL_ALIAS) {
        return ResolvedDashboardBaseUrl {
            value,
            source: "env:ENVELOPE_DASHBOARD_URL",
        };
    }

    if let Ok(Some(value)) = persistent_dashboard_base_url() {
        return ResolvedDashboardBaseUrl {
            value,
            source: "config",
        };
    }

    ResolvedDashboardBaseUrl {
        value: super::ui::DEFAULT_DASHBOARD_BASE.to_string(),
        source: "default",
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

fn dashboard_base_url_from_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .and_then(|value| normalize_dashboard_base(&value))
}

fn normalize_dashboard_base(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.trim_end_matches('/').to_string())
}

fn require_supported_key(key: &str) -> Result<()> {
    if key == DASHBOARD_BASE_URL_KEY {
        Ok(())
    } else {
        bail!("unknown config key `{key}`; supported key: {DASHBOARD_BASE_URL_KEY}")
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
    use super::{ENV_DASHBOARD_BASE_URL, ENV_DASHBOARD_URL_ALIAS};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    thread_local! {
        static TEST_CONFIG_FILE_PATH: RefCell<Option<PathBuf>> = RefCell::new(None);
    }

    static TEST_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    pub(crate) struct DashboardConfigTestGuard {
        _guard: MutexGuard<'static, ()>,
        prev_primary: Option<String>,
        prev_alias: Option<String>,
    }

    impl DashboardConfigTestGuard {
        pub(crate) fn set_primary_env(&self, value: &str) {
            unsafe { std::env::set_var(ENV_DASHBOARD_BASE_URL, value) };
        }

        pub(crate) fn set_alias_env(&self, value: &str) {
            unsafe { std::env::set_var(ENV_DASHBOARD_URL_ALIAS, value) };
        }
    }

    impl Drop for DashboardConfigTestGuard {
        fn drop(&mut self) {
            match &self.prev_primary {
                Some(value) => unsafe { std::env::set_var(ENV_DASHBOARD_BASE_URL, value) },
                None => unsafe { std::env::remove_var(ENV_DASHBOARD_BASE_URL) },
            }
            match &self.prev_alias {
                Some(value) => unsafe { std::env::set_var(ENV_DASHBOARD_URL_ALIAS, value) },
                None => unsafe { std::env::remove_var(ENV_DASHBOARD_URL_ALIAS) },
            }
            TEST_CONFIG_FILE_PATH.with(|path| *path.borrow_mut() = None);
        }
    }

    pub(crate) fn isolated_dashboard_config(path: PathBuf) -> DashboardConfigTestGuard {
        let guard = TEST_ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("dashboard config test env lock poisoned");
        let prev_primary = std::env::var(ENV_DASHBOARD_BASE_URL).ok();
        let prev_alias = std::env::var(ENV_DASHBOARD_URL_ALIAS).ok();
        unsafe { std::env::remove_var(ENV_DASHBOARD_BASE_URL) };
        unsafe { std::env::remove_var(ENV_DASHBOARD_URL_ALIAS) };
        TEST_CONFIG_FILE_PATH.with(|slot| *slot.borrow_mut() = Some(path));
        DashboardConfigTestGuard {
            _guard: guard,
            prev_primary,
            prev_alias,
        }
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
    fn resolves_primary_env_before_alias_and_config() {
        let path = test_config_path("env-precedence");
        let _ = fs::remove_file(&path);
        let guard = isolated_dashboard_config(path.clone());
        write_config_value(
            &path,
            &json!({"dashboard": {"base_url": "https://config.example/"}}),
        )
        .unwrap();
        guard.set_alias_env("https://alias.example/");
        guard.set_primary_env("https://primary.example/");

        let resolved = resolved_dashboard_base_url();
        assert_eq!(resolved.value, "https://primary.example");
        assert_eq!(resolved.source, "env:ENVELOPE_DASHBOARD_BASE_URL");
    }

    #[test]
    fn resolves_alias_env_before_config() {
        let path = test_config_path("alias-precedence");
        let _ = fs::remove_file(&path);
        let guard = isolated_dashboard_config(path.clone());
        write_config_value(
            &path,
            &json!({"dashboard": {"base_url": "https://config.example/"}}),
        )
        .unwrap();
        guard.set_alias_env("https://alias.example/");

        let resolved = resolved_dashboard_base_url();
        assert_eq!(resolved.value, "https://alias.example");
        assert_eq!(resolved.source, "env:ENVELOPE_DASHBOARD_URL");
    }

    #[test]
    fn resolves_persistent_config_before_default() {
        let path = test_config_path("config-default");
        let _ = fs::remove_file(&path);
        let _guard = isolated_dashboard_config(path.clone());
        write_config_value(
            &path,
            &json!({"dashboard": {"base_url": "https://config.example/"}}),
        )
        .unwrap();

        let resolved = resolved_dashboard_base_url();
        assert_eq!(resolved.value, "https://config.example");
        assert_eq!(resolved.source, "config");
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
}
