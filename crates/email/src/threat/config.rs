// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Threat-engine configuration, read from the same `config.json` in the app
//! data directory that `envelope config` writes.
//!
//! | key | default |
//! |---|---|
//! | `threat.enabled` | `true` |
//! | `threat.quarantine` | `tag` (`none`, `tag`, `move`) |
//! | `threat.on_read` | `true` (scan on open when no current verdict) |
//! | `threat.report_to` | `reportphishing@apwg.org` |
//! | `threat.analyzers.<name>` | `true` |
//! | `threat.reputation.provider` | `off` (`off`, `spamhaus-dbl`) |
//! | `threat.reputation.dqs_key` | unset (env `ENVELOPE_REPUTATION_API_KEY`) |
//! | `threat.clamd.address` | unset = off (`unix:/path` or `tcp:host:port`) |
//! | `threat.clamd.required` | `false` |
//! | `threat.receiver_domain` | unset (`<account>=<domain>[,...]`; see [`ThreatConfig::receiver_domain`]) |
//! | `sync.poll_interval_secs` | `300` |
//!
//! A present-but-invalid value is an error, never a silent default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::domains::ascii_host;
use super::{ANALYZER_NAMES, is_dns_name};

pub const CONFIG_FILE_NAME: &str = "config.json";
pub const DEFAULT_REPORT_TO: &str = "reportphishing@apwg.org";
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 300;
/// Shortest accepted poll interval; each tick opens IMAP connections.
pub const MIN_POLL_INTERVAL_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quarantine {
    /// Record the verdict only.
    None,
    /// Tag dangerous mail `threat:quarantined`; nothing moves.
    Tag,
    /// Tag, then move dangerous mail through the shipped quarantine rule.
    Move,
}

impl Quarantine {
    pub fn as_str(self) -> &'static str {
        match self {
            Quarantine::None => "none",
            Quarantine::Tag => "tag",
            Quarantine::Move => "move",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        Ok(match raw.trim() {
            "none" => Quarantine::None,
            "tag" => Quarantine::Tag,
            "move" => Quarantine::Move,
            other => bail!("threat.quarantine must be none, tag, or move (got `{other}`)"),
        })
    }
}

/// Env fallback for `threat.reputation.dqs_key`, so the key can stay out of
/// `config.json`.
pub const REPUTATION_KEY_ENV: &str = "ENVELOPE_REPUTATION_API_KEY";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReputationProvider {
    Off,
    /// Spamhaus Domain Block List over DNS.
    SpamhausDbl,
}

impl ReputationProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            ReputationProvider::Off => "off",
            ReputationProvider::SpamhausDbl => "spamhaus-dbl",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        Ok(match raw.trim() {
            "off" => ReputationProvider::Off,
            "spamhaus-dbl" => ReputationProvider::SpamhausDbl,
            other => {
                bail!("threat.reputation.provider must be off or spamhaus-dbl (got `{other}`)")
            }
        })
    }
}

/// Where clamd listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClamdAddress {
    Unix(PathBuf),
    /// `host:port`.
    Tcp(String),
}

impl ClamdAddress {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if let Some(path) = raw.strip_prefix("unix:") {
            if !path.starts_with('/') {
                bail!("threat.clamd.address unix: needs an absolute socket path (got `{raw}`)");
            }
            return Ok(ClamdAddress::Unix(PathBuf::from(path)));
        }
        if let Some(hostport) = raw.strip_prefix("tcp:") {
            let port_ok = hostport
                .rsplit_once(':')
                .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok());
            if !port_ok {
                bail!("threat.clamd.address tcp: needs host:port (got `{raw}`)");
            }
            return Ok(ClamdAddress::Tcp(hostport.to_string()));
        }
        bail!(
            "threat.clamd.address must be unix:/path/to/clamd.sock or tcp:host:port (got `{raw}`)"
        )
    }

    pub fn display(&self) -> String {
        match self {
            ClamdAddress::Unix(path) => format!("unix:{}", path.display()),
            ClamdAddress::Tcp(hostport) => format!("tcp:{hostport}"),
        }
    }
}

/// A Spamhaus DQS key is one DNS label.
fn valid_dqs_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 63 && key.chars().all(|c| c.is_ascii_alphanumeric())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreatConfig {
    pub enabled: bool,
    pub quarantine: Quarantine,
    pub on_read: bool,
    pub report_to: String,
    /// Explicit per-analyzer switches; absent means enabled.
    pub analyzers: BTreeMap<String, bool>,
    pub poll_interval_secs: u64,
    pub reputation_provider: ReputationProvider,
    /// Spamhaus DQS key from config; see [`ThreatConfig::dqs_key`].
    pub dqs_key: Option<String>,
    /// `None` means clamd scanning is off.
    pub clamd: Option<ClamdAddress>,
    /// A clamd error makes the verdict `unavailable` instead of being
    /// recorded as a skipped analyzer.
    pub clamd_required: bool,
    /// The operator's receiving mail domain per account address, for
    /// accounts whose IMAP host is not in the domain of the hosts that accept
    /// their mail. Values are registrable domains.
    pub receiver_domains: BTreeMap<String, String>,
}

/// Providers whose mail is accepted by hosts in another registrable domain
/// than their IMAP host. Only entries checked against the provider's own
/// headers or documentation belong here.
const PROVIDER_RECEIVER_DOMAINS: &[(&str, &str)] = &[
    // IMAP imap.gmail.com; mail accepted by mx.google.com, which is also the
    // authserv-id of its Authentication-Results.
    ("gmail.com", "google.com"),
];

/// The registrable domain of a DNS host name; `None` for an IP literal, a
/// single label, or a host with no registrable domain.
fn registrable_dns_domain(host: &str) -> Option<String> {
    let host = ascii_host(host);
    if !is_dns_name(&host) {
        return None;
    }
    psl::domain_str(&host).map(str::to_string)
}

impl Default for ThreatConfig {
    fn default() -> Self {
        ThreatConfig {
            enabled: true,
            quarantine: Quarantine::Tag,
            on_read: true,
            report_to: DEFAULT_REPORT_TO.to_string(),
            analyzers: BTreeMap::new(),
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            reputation_provider: ReputationProvider::Off,
            dqs_key: None,
            clamd: None,
            clamd_required: false,
            receiver_domains: BTreeMap::new(),
        }
    }
}

/// Every config key this module owns, for `envelope config` validation.
pub fn is_threat_key(key: &str) -> bool {
    matches!(
        key,
        "threat.enabled"
            | "threat.quarantine"
            | "threat.on_read"
            | "threat.report_to"
            | "threat.reputation.provider"
            | "threat.reputation.dqs_key"
            | "threat.clamd.address"
            | "threat.clamd.required"
            | "sync.poll_interval_secs"
            | "threat.receiver_domain"
    ) || key
        .strip_prefix("threat.analyzers.")
        .is_some_and(|name| ANALYZER_NAMES.contains(&name))
}

/// JSON pointer for a dotted key (`threat.analyzers.links` →
/// `/threat/analyzers/links`).
pub fn pointer_for(key: &str) -> String {
    format!("/{}", key.replace('.', "/"))
}

/// Parse the string a user typed into the typed JSON value stored.
pub fn parse_value(key: &str, raw: &str) -> Result<Value> {
    let raw = raw.trim();
    let parse_bool = || -> Result<Value> {
        match raw {
            "true" | "on" | "yes" | "1" => Ok(Value::Bool(true)),
            "false" | "off" | "no" | "0" => Ok(Value::Bool(false)),
            other => bail!("{key} must be true or false (got `{other}`)"),
        }
    };
    match key {
        "threat.enabled" | "threat.on_read" | "threat.clamd.required" => parse_bool(),
        "threat.reputation.provider" => Ok(Value::String(
            ReputationProvider::parse(raw)?.as_str().to_string(),
        )),
        "threat.reputation.dqs_key" => {
            if !valid_dqs_key(raw) {
                bail!(
                    "threat.reputation.dqs_key must be the letters and digits of a Spamhaus DQS key"
                );
            }
            Ok(Value::String(raw.to_string()))
        }
        "threat.clamd.address" => Ok(Value::String(ClamdAddress::parse(raw)?.display())),
        _ if key.starts_with("threat.analyzers.") => parse_bool(),
        "threat.quarantine" => Ok(Value::String(Quarantine::parse(raw)?.as_str().to_string())),
        "threat.report_to" => {
            let at = raw.find('@');
            if raw.contains(char::is_whitespace) || at.is_none_or(|i| i == 0 || i == raw.len() - 1)
            {
                bail!("threat.report_to must be one email address (got `{raw}`)");
            }
            Ok(Value::String(raw.to_string()))
        }
        "threat.receiver_domain" => {
            let mut map = serde_json::Map::new();
            for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                let (account, domain) = entry
                    .split_once('=')
                    .map(|(a, d)| (a.trim().to_lowercase(), d.trim()))
                    .filter(|(a, _)| a.contains('@'))
                    .with_context(|| {
                        format!("threat.receiver_domain entries are <account address>=<domain> (got `{entry}`)")
                    })?;
                map.insert(account, Value::String(parse_receiver_domain(domain)?));
            }
            if map.is_empty() {
                bail!("threat.receiver_domain needs <account address>=<domain>[,...]");
            }
            Ok(Value::Object(map))
        }
        "sync.poll_interval_secs" => {
            let secs: u64 = raw.parse().with_context(|| {
                format!("sync.poll_interval_secs must be seconds (got `{raw}`)")
            })?;
            if secs < MIN_POLL_INTERVAL_SECS {
                bail!("sync.poll_interval_secs must be at least {MIN_POLL_INTERVAL_SECS}");
            }
            Ok(Value::from(secs))
        }
        other => bail!("`{other}` is not a threat or sync config key"),
    }
}

fn parse_receiver_domain(raw: &str) -> Result<String> {
    registrable_dns_domain(raw).with_context(|| {
        format!("threat.receiver_domain needs a DNS domain such as example.org (got `{raw}`)")
    })
}

impl ThreatConfig {
    /// The registrable domain of the hosts that accept `account_address`'s
    /// mail: the operator's `threat.receiver_domain` entry for it, else its
    /// IMAP host's, mapped for providers that accept mail under another
    /// domain. `None` when neither gives a DNS domain; Authentication-Results
    /// are then never trusted.
    pub fn receiver_domain(
        &self,
        account_address: &str,
        imap_host: Option<&str>,
    ) -> Option<String> {
        if let Some(domain) = self
            .receiver_domains
            .get(&account_address.trim().to_lowercase())
        {
            return Some(domain.clone());
        }
        let domain = registrable_dns_domain(imap_host?)?;
        Some(
            PROVIDER_RECEIVER_DOMAINS
                .iter()
                .find(|(imap, _)| *imap == domain)
                .map_or(domain, |(_, mail)| mail.to_string()),
        )
    }

    pub fn analyzer_enabled(&self, name: &str) -> bool {
        self.analyzers.get(name).copied().unwrap_or(true)
    }

    /// The DQS key: `threat.reputation.dqs_key`, else
    /// `ENVELOPE_REPUTATION_API_KEY`.
    pub fn dqs_key(&self) -> Result<Option<String>> {
        if let Some(key) = &self.dqs_key {
            return Ok(Some(key.clone()));
        }
        match std::env::var(REPUTATION_KEY_ENV) {
            Ok(key) if key.trim().is_empty() => Ok(None),
            Ok(key) => {
                let key = key.trim().to_string();
                if !valid_dqs_key(&key) {
                    bail!(
                        "{REPUTATION_KEY_ENV} must be the letters and digits of a Spamhaus DQS key"
                    );
                }
                Ok(Some(key))
            }
            Err(_) => Ok(None),
        }
    }

    /// Build from the whole `config.json` object.
    pub fn from_config_value(config: &Value) -> Result<Self> {
        let mut out = ThreatConfig::default();
        let bool_at = |key: &str| -> Result<Option<bool>> {
            match config.pointer(&pointer_for(key)) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Bool(b)) => Ok(Some(*b)),
                Some(other) => bail!("{key} must be a boolean in config.json (found {other})"),
            }
        };
        if let Some(v) = bool_at("threat.enabled")? {
            out.enabled = v;
        }
        if let Some(v) = bool_at("threat.on_read")? {
            out.on_read = v;
        }
        match config.pointer("/threat/quarantine") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) => out.quarantine = Quarantine::parse(s)?,
            Some(other) => {
                bail!("threat.quarantine must be a string in config.json (found {other})")
            }
        }
        match config.pointer("/threat/report_to") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) => {
                out.report_to = parse_value("threat.report_to", s)?
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            }
            Some(other) => {
                bail!("threat.report_to must be a string in config.json (found {other})")
            }
        }
        let string_at = |key: &str| -> Result<Option<String>> {
            match config.pointer(&pointer_for(key)) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(
                    parse_value(key, s)?
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )),
                Some(other) => bail!("{key} must be a string in config.json (found {other})"),
            }
        };
        if let Some(v) = string_at("threat.reputation.provider")? {
            out.reputation_provider = ReputationProvider::parse(&v)?;
        }
        out.dqs_key = string_at("threat.reputation.dqs_key")?;
        if let Some(v) = string_at("threat.clamd.address")? {
            out.clamd = Some(ClamdAddress::parse(&v)?);
        }
        if let Some(v) = bool_at("threat.clamd.required")? {
            out.clamd_required = v;
        }
        if let Some(analyzers) = config.pointer("/threat/analyzers") {
            let map = analyzers
                .as_object()
                .context("threat.analyzers must be an object in config.json")?;
            for (name, value) in map {
                if !ANALYZER_NAMES.contains(&name.as_str()) {
                    bail!(
                        "threat.analyzers.{name} is not an analyzer; known: {}",
                        ANALYZER_NAMES.join(", ")
                    );
                }
                let enabled = value
                    .as_bool()
                    .with_context(|| format!("threat.analyzers.{name} must be a boolean"))?;
                out.analyzers.insert(name.clone(), enabled);
            }
        }
        if let Some(entries) = config.pointer("/threat/receiver_domain") {
            let map = entries
                .as_object()
                .context("threat.receiver_domain must be an object in config.json")?;
            for (account, domain) in map {
                let domain = domain.as_str().with_context(|| {
                    format!("threat.receiver_domain.{account} must be a string")
                })?;
                out.receiver_domains
                    .insert(account.to_lowercase(), parse_receiver_domain(domain)?);
            }
        }
        match config.pointer("/sync/poll_interval_secs") {
            None | Some(Value::Null) => {}
            Some(v) => {
                let secs = v
                    .as_u64()
                    .context("sync.poll_interval_secs must be a whole number of seconds")?;
                if secs < MIN_POLL_INTERVAL_SECS {
                    bail!("sync.poll_interval_secs must be at least {MIN_POLL_INTERVAL_SECS}");
                }
                out.poll_interval_secs = secs;
            }
        }
        Ok(out)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        if contents.trim().is_empty() {
            return Ok(Self::default());
        }
        let value: Value = serde_json::from_str(&contents)
            .with_context(|| format!("parse JSON config {}", path.display()))?;
        Self::from_config_value(&value)
            .with_context(|| format!("invalid threat config in {}", path.display()))
    }

    /// Load from `<app data dir>/config.json`.
    pub fn load() -> Result<Self> {
        Self::load_from(&envelope_email_store::app_data_dir().join(CONFIG_FILE_NAME))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn receiver_domain_comes_from_the_operator_or_the_imap_host() {
        let c = ThreatConfig::default();
        assert_eq!(
            c.receiver_domain("me@gmail.com", Some("imap.gmail.com"))
                .as_deref(),
            Some("google.com")
        );
        assert_eq!(
            c.receiver_domain("a@example.net", Some("imap.migadu.com"))
                .as_deref(),
            Some("migadu.com")
        );
        assert_eq!(
            c.receiver_domain("a@bank.co.uk", Some("Mail.Bank.CO.UK."))
                .as_deref(),
            Some("bank.co.uk")
        );
        for host in ["127.0.0.1", "::1", "[10.0.0.5]", "localhost", "mail", ""] {
            assert_eq!(c.receiver_domain("a@b.example", Some(host)), None, "{host}");
        }
        assert_eq!(c.receiver_domain("a@b.example", None), None);

        let value = parse_value(
            "threat.receiver_domain",
            "Me@Self.Host=mx.self.host, other@x.example=mail.x.example",
        )
        .unwrap();
        let c = ThreatConfig::from_config_value(&json!({"threat": {"receiver_domain": value}}))
            .unwrap();
        assert_eq!(
            c.receiver_domain("me@self.host", Some("127.0.0.1"))
                .as_deref(),
            Some("self.host")
        );
        assert_eq!(
            c.receiver_domain("OTHER@x.example", Some("imap.gmail.com"))
                .as_deref(),
            Some("x.example")
        );
        for bad in [
            "me@self.host",
            "me@self.host=10.0.0.1",
            "=self.host",
            "me@self.host=mail",
            "",
        ] {
            assert!(parse_value("threat.receiver_domain", bad).is_err(), "{bad}");
        }
        assert!(
            ThreatConfig::from_config_value(
                &json!({"threat": {"receiver_domain": {"a@b.example": "10.0.0.1"}}})
            )
            .is_err()
        );
    }

    #[test]
    fn defaults_when_nothing_is_configured() {
        let c = ThreatConfig::from_config_value(&json!({"dashboard": {"base_url": "x"}})).unwrap();
        assert_eq!(c, ThreatConfig::default());
        assert_eq!(c.quarantine, Quarantine::Tag);
        assert_eq!(c.report_to, DEFAULT_REPORT_TO);
        assert_eq!(c.poll_interval_secs, 300);
        assert!(c.analyzer_enabled("links"));
    }

    #[test]
    fn typed_values_round_trip_through_pointers() {
        let mut config = json!({});
        for (key, raw) in [
            ("threat.quarantine", "move"),
            ("threat.on_read", "false"),
            ("threat.analyzers.content", "off"),
            ("sync.poll_interval_secs", "120"),
            ("threat.report_to", "abuse@example.org"),
        ] {
            let value = parse_value(key, raw).unwrap();
            let parts: Vec<&str> = key.split('.').collect();
            let mut cursor = &mut config;
            for part in &parts[..parts.len() - 1] {
                cursor = cursor
                    .as_object_mut()
                    .unwrap()
                    .entry(part.to_string())
                    .or_insert_with(|| json!({}));
            }
            cursor[parts[parts.len() - 1]] = value;
        }
        let c = ThreatConfig::from_config_value(&config).unwrap();
        assert_eq!(c.quarantine, Quarantine::Move);
        assert!(!c.on_read);
        assert!(!c.analyzer_enabled("content"));
        assert_eq!(c.poll_interval_secs, 120);
        assert_eq!(c.report_to, "abuse@example.org");
    }

    #[test]
    fn invalid_values_fail_loud() {
        assert!(parse_value("threat.quarantine", "delete").is_err());
        assert!(parse_value("sync.poll_interval_secs", "5").is_err());
        assert!(parse_value("threat.report_to", "not an address").is_err());
        assert!(parse_value("threat.enabled", "maybe").is_err());
        assert!(ThreatConfig::from_config_value(&json!({"threat": {"enabled": "yes"}})).is_err());
        assert!(
            ThreatConfig::from_config_value(&json!({"threat": {"analyzers": {"clam": true}}}))
                .is_err()
        );
    }

    #[test]
    fn optional_analyzers_are_off_by_default_and_parse_when_set() {
        let c = ThreatConfig::default();
        assert_eq!(c.reputation_provider, ReputationProvider::Off);
        assert_eq!(c.clamd, None);
        assert!(!c.clamd_required);

        let c = ThreatConfig::from_config_value(&json!({"threat": {
            "reputation": {"provider": "spamhaus-dbl", "dqs_key": "abc123"},
            "clamd": {"address": "unix:/opt/homebrew/var/run/clamav/clamd.sock", "required": true},
        }}))
        .unwrap();
        assert_eq!(c.reputation_provider, ReputationProvider::SpamhausDbl);
        assert_eq!(c.dqs_key().unwrap().as_deref(), Some("abc123"));
        assert_eq!(
            c.clamd,
            Some(ClamdAddress::Unix(PathBuf::from(
                "/opt/homebrew/var/run/clamav/clamd.sock"
            )))
        );
        assert!(c.clamd_required);

        assert_eq!(
            ClamdAddress::parse("tcp:127.0.0.1:3310").unwrap(),
            ClamdAddress::Tcp("127.0.0.1:3310".into())
        );
        assert!(ClamdAddress::parse("tcp:localhost").is_err());
        assert!(ClamdAddress::parse("unix:relative.sock").is_err());
        assert!(ClamdAddress::parse("/var/run/clamd.sock").is_err());
        assert!(parse_value("threat.reputation.provider", "virustotal").is_err());
        assert!(parse_value("threat.reputation.dqs_key", "a.b").is_err());
        assert!(
            ThreatConfig::from_config_value(&json!({"threat": {"clamd": {"required": "yes"}}}))
                .is_err()
        );
    }

    #[test]
    fn key_catalog() {
        assert!(is_threat_key("threat.analyzers.ledger"));
        assert!(is_threat_key("sync.poll_interval_secs"));
        assert!(is_threat_key("threat.clamd.address"));
        assert!(is_threat_key("threat.reputation.provider"));
        assert!(!is_threat_key("threat.analyzers.clamd"));
        assert!(!is_threat_key("dashboard.base_url"));
        assert_eq!(
            pointer_for("threat.analyzers.links"),
            "/threat/analyzers/links"
        );
    }
}
