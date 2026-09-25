// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Decisions provider configuration, read from the `config.json` that
//! `envelope config` writes.
//!
//! | key | default |
//! |---|---|
//! | `decisions.provider` | `openrouter` (`openrouter`, `laya`, `custom`) |
//! | `decisions.base_url` | `https://openrouter.ai/api/alpha/decisions` (required for `custom`) |
//! | `decisions.model` | `typesafe/jev-1.13` (required for `custom`) |
//! | `decisions.key_env` | `OPENROUTER_API_KEY` |
//! | `decisions.propose_actions` | `false` |
//! | `decisions.route_actions.<route>` | see [`default_route_actions`] |
//!
//! Nothing here turns a decision model on. The mail engine runs only when
//! invoked (`envelope engine`), and the threat analyzer only with
//! `threat.analyzers.jev = true`; this module says where their requests go.
//! Laya always uses its fixed loopback endpoint and pinned model, so
//! `base_url`, `model` and `key_env` are refused with it rather than ignored.
//!
//! A present-but-invalid value is an error, never a silent default.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::jev::{
    DEFAULT_KEY_ENV, DecisionsProvider, JEV_MODEL, JevBackend, MailRoute,
    OPENROUTER_DECISIONS_ENDPOINT, PolicyDecision, ROUTE_OPTIONS,
};
use crate::rules::ConfirmableAction;

pub const CONFIG_FILE_NAME: &str = "config.json";

/// Every scalar key this module owns (route actions are
/// `decisions.route_actions.<route>`).
pub const KEYS: &[&str] = &[
    "decisions.provider",
    "decisions.base_url",
    "decisions.model",
    "decisions.key_env",
    "decisions.propose_actions",
];

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionsConfig {
    pub provider: DecisionsProvider,
    /// Mint Confirm offers from confident `route` verdicts. Offers are never
    /// executed without a human confirming them.
    pub propose_actions: bool,
    /// Route token to the action an offer proposes; `None` proposes nothing.
    pub route_actions: BTreeMap<String, Option<ConfirmableAction>>,
}

impl Default for DecisionsConfig {
    fn default() -> Self {
        DecisionsConfig {
            provider: DecisionsProvider::openrouter(),
            propose_actions: false,
            route_actions: default_route_actions(),
        }
    }
}

/// The shipped route → action table. Tags only: reversible, local, and
/// visible in the reader. `junk`, `routine` and `review` propose nothing;
/// junk handling belongs to the threat engine and `engine --apply`.
pub fn default_route_actions() -> BTreeMap<String, Option<ConfirmableAction>> {
    let tag = |t: &str| Some(ConfirmableAction::AddTag(t.to_string()));
    ROUTE_OPTIONS
        .iter()
        .map(|route| {
            let action = match *route {
                "follow_up" => tag("follow-up"),
                "important" => tag("important"),
                "digest_news" => tag("digest"),
                "unsubscribe_candidate" => tag("unsubscribe-candidate"),
                _ => None,
            };
            (route.to_string(), action)
        })
        .collect()
}

/// True for any key `envelope config` should route here.
pub fn is_decisions_key(key: &str) -> bool {
    KEYS.contains(&key)
        || key
            .strip_prefix("decisions.route_actions.")
            .is_some_and(|route| ROUTE_OPTIONS.contains(&route))
}

/// Parse `add_tag:<tag>`, `flag:<flag>`, `move:<folder>` or `none`. The
/// [`ConfirmableAction`] allowlist runs, so a move into Trash or Junk fails.
pub fn parse_route_action(raw: &str) -> Result<Option<ConfirmableAction>> {
    let raw = raw.trim();
    if raw == "none" {
        return Ok(None);
    }
    let (kind, arg) = raw.split_once(':').with_context(|| {
        format!(
            "route action must be add_tag:<tag>, flag:<flag>, move:<folder> or none (got `{raw}`)"
        )
    })?;
    let arg = arg.trim().to_string();
    let action = match kind.trim() {
        "add_tag" => ConfirmableAction::AddTag(arg),
        "flag" => ConfirmableAction::Flag(arg),
        "move" => ConfirmableAction::Move(arg),
        other => bail!("route action kind must be add_tag, flag or move (got `{other}`)"),
    };
    action.validate().map_err(anyhow::Error::msg)?;
    Ok(Some(action))
}

/// The config string for a route action, the inverse of
/// [`parse_route_action`].
pub fn route_action_label(action: Option<&ConfirmableAction>) -> String {
    match action {
        None => "none".to_string(),
        Some(ConfirmableAction::AddTag(t)) => format!("add_tag:{t}"),
        Some(ConfirmableAction::Flag(f)) => format!("flag:{f}"),
        Some(ConfirmableAction::Move(m)) => format!("move:{m}"),
    }
}

fn valid_key_env(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// Parse the string a user typed into the typed JSON value stored.
pub fn parse_value(key: &str, raw: &str) -> Result<Value> {
    let raw = raw.trim();
    match key {
        "decisions.provider" => {
            let backend = JevBackend::parse(raw).with_context(|| {
                format!("decisions.provider must be openrouter, laya or custom (got `{raw}`)")
            })?;
            Ok(Value::String(backend.as_str().to_string()))
        }
        "decisions.base_url" => {
            let url = url::Url::parse(raw)
                .with_context(|| format!("decisions.base_url is not a URL (got `{raw}`)"))?;
            if url.scheme() != "https" || url.host_str().is_none() {
                bail!("decisions.base_url must be an https URL (got `{raw}`)");
            }
            if !url.username().is_empty() || url.password().is_some() {
                bail!("decisions.base_url must not carry credentials; use decisions.key_env");
            }
            Ok(Value::String(raw.to_string()))
        }
        "decisions.model" => {
            if raw.is_empty() || raw.contains(char::is_whitespace) || raw.len() > 200 {
                bail!("decisions.model must be one model id such as {JEV_MODEL}");
            }
            Ok(Value::String(raw.to_string()))
        }
        "decisions.key_env" => {
            if !valid_key_env(raw) {
                bail!(
                    "decisions.key_env must name an environment variable (A-Z, 0-9, _), not hold the key"
                );
            }
            Ok(Value::String(raw.to_string()))
        }
        "decisions.propose_actions" => match raw {
            "true" | "on" | "yes" | "1" => Ok(Value::Bool(true)),
            "false" | "off" | "no" | "0" => Ok(Value::Bool(false)),
            other => bail!("decisions.propose_actions must be true or false (got `{other}`)"),
        },
        _ if is_decisions_key(key) => {
            let action = parse_route_action(raw).with_context(|| format!("invalid {key}"))?;
            Ok(Value::String(route_action_label(action.as_ref())))
        }
        other => bail!("`{other}` is not a decisions config key"),
    }
}

impl DecisionsConfig {
    /// Build from the whole `config.json` object.
    pub fn from_config_value(config: &Value) -> Result<Self> {
        let mut out = DecisionsConfig::default();
        let Some(section) = config.get("decisions") else {
            return Ok(out);
        };
        let section = section
            .as_object()
            .context("decisions must be an object in config.json")?;
        for key in section.keys() {
            if !matches!(
                key.as_str(),
                "provider" | "base_url" | "model" | "key_env" | "propose_actions" | "route_actions"
            ) {
                bail!("decisions.{key} is not a decisions config key");
            }
        }
        let string_at = |name: &str| -> Result<Option<String>> {
            let key = format!("decisions.{name}");
            match section.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(
                    parse_value(&key, s)?
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )),
                Some(other) => bail!("{key} must be a string in config.json (found {other})"),
            }
        };
        let backend = match string_at("provider")? {
            Some(p) => JevBackend::parse(&p).expect("parse_value accepted it"),
            None => JevBackend::Openrouter,
        };
        let base_url = string_at("base_url")?;
        let model = string_at("model")?;
        let key_env = string_at("key_env")?;
        out.provider = match backend {
            JevBackend::Laya => {
                for (name, set) in [
                    ("base_url", base_url.is_some()),
                    ("model", model.is_some()),
                    ("key_env", key_env.is_some()),
                ] {
                    if set {
                        bail!(
                            "decisions.{name} does not apply to provider laya, which always uses its pinned model on 127.0.0.1:8791; unset it"
                        );
                    }
                }
                DecisionsProvider::laya()
            }
            JevBackend::Openrouter => DecisionsProvider {
                backend,
                base_url: base_url.unwrap_or_else(|| OPENROUTER_DECISIONS_ENDPOINT.to_string()),
                model: model.unwrap_or_else(|| JEV_MODEL.to_string()),
                key_env: Some(key_env.unwrap_or_else(|| DEFAULT_KEY_ENV.to_string())),
            },
            JevBackend::Custom => DecisionsProvider {
                backend,
                base_url: base_url.context("decisions.provider custom needs decisions.base_url")?,
                model: model.context("decisions.provider custom needs decisions.model")?,
                key_env: Some(key_env.unwrap_or_else(|| DEFAULT_KEY_ENV.to_string())),
            },
        };
        match section.get("propose_actions") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(b)) => out.propose_actions = *b,
            Some(other) => {
                bail!("decisions.propose_actions must be a boolean in config.json (found {other})")
            }
        }
        if let Some(table) = section.get("route_actions") {
            let table = table
                .as_object()
                .context("decisions.route_actions must be an object in config.json")?;
            for (route, value) in table {
                if !ROUTE_OPTIONS.contains(&route.as_str()) {
                    bail!(
                        "decisions.route_actions.{route} is not a route; known: {}",
                        ROUTE_OPTIONS.join(", ")
                    );
                }
                let raw = value
                    .as_str()
                    .with_context(|| format!("decisions.route_actions.{route} must be a string"))?;
                let action = parse_route_action(raw)
                    .with_context(|| format!("invalid decisions.route_actions.{route}"))?;
                out.route_actions.insert(route.clone(), action);
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
            .with_context(|| format!("invalid decisions config in {}", path.display()))
    }

    /// Load from `<app data dir>/config.json`.
    pub fn load() -> Result<Self> {
        Self::load_from(&envelope_email_store::app_data_dir().join(CONFIG_FILE_NAME))
    }

    /// The provider for one run. `requested` (a `--jev-backend` flag) wins
    /// when given: the configured provider when it names the same backend,
    /// otherwise that backend's defaults. `custom` has no defaults, so it
    /// must be the configured provider.
    pub fn provider_for(&self, requested: Option<JevBackend>) -> Result<DecisionsProvider> {
        match requested {
            None => Ok(self.provider.clone()),
            Some(backend) if backend == self.provider.backend => Ok(self.provider.clone()),
            Some(JevBackend::Laya) => Ok(DecisionsProvider::laya()),
            Some(JevBackend::Openrouter) => Ok(DecisionsProvider::openrouter()),
            Some(JevBackend::Custom) => {
                bail!("--jev-backend custom needs decisions.provider custom in config.json")
            }
        }
    }

    /// The offer a confident route verdict proposes, if any: `None` when
    /// proposals are off, the policy abstained (the route did not clear
    /// [`crate::jev::apply_policy`]'s thresholds), the route is `review`, or
    /// the table maps the route to nothing.
    pub fn offer_for(&self, policy: &PolicyDecision) -> Option<(String, Vec<ConfirmableAction>)> {
        if !self.propose_actions || policy.abstained || policy.route == MailRoute::Review {
            return None;
        }
        let route = policy.route.as_str();
        let action = self.route_actions.get(route)?.clone()?;
        action.validate().ok()?;
        let what = match &action {
            ConfirmableAction::AddTag(t) => format!("Add the tag {t}"),
            ConfirmableAction::Flag(f) => format!("Flag it {f}"),
            ConfirmableAction::Move(m) => format!("Move it to {m}"),
        };
        let prompt = format!("Jev reads this as {}. {what}?", route.replace('_', " "));
        Some((prompt, vec![action]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::Urgency;
    use serde_json::json;

    fn policy(route: MailRoute, abstained: bool) -> PolicyDecision {
        PolicyDecision {
            route,
            urgency: Urgency::NotUrgent,
            notify_user_now: false,
            requires_reply: false,
            bulk_or_subscription: false,
            abstained,
        }
    }

    #[test]
    fn defaults_point_at_openrouter_jev_with_proposals_off() {
        let c = DecisionsConfig::from_config_value(&json!({"threat": {"enabled": true}})).unwrap();
        assert_eq!(c, DecisionsConfig::default());
        assert_eq!(c.provider.backend, JevBackend::Openrouter);
        assert_eq!(
            c.provider.base_url,
            "https://openrouter.ai/api/alpha/decisions"
        );
        assert_eq!(c.provider.model, "typesafe/jev-1.13");
        assert_eq!(c.provider.key_env.as_deref(), Some("OPENROUTER_API_KEY"));
        assert!(!c.propose_actions);
        assert_eq!(
            c.route_actions["follow_up"],
            Some(ConfirmableAction::AddTag("follow-up".into()))
        );
        assert_eq!(c.route_actions["junk"], None);
    }

    #[test]
    fn custom_and_overrides_parse_and_custom_requires_url_and_model() {
        let c = DecisionsConfig::from_config_value(&json!({"decisions": {
            "provider": "custom",
            "base_url": "https://decide.example.net/v1/decisions",
            "model": "acme/judge-2",
            "key_env": "ACME_KEY",
            "propose_actions": true,
            "route_actions": {"follow_up": "move:Follow up", "digest_news": "none"},
        }}))
        .unwrap();
        assert_eq!(c.provider.backend, JevBackend::Custom);
        assert_eq!(c.provider.model, "acme/judge-2");
        assert_eq!(c.provider.key_env.as_deref(), Some("ACME_KEY"));
        assert!(c.propose_actions);
        assert_eq!(
            c.route_actions["follow_up"],
            Some(ConfirmableAction::Move("Follow up".into()))
        );
        assert_eq!(c.route_actions["digest_news"], None);

        for missing in [
            json!({"decisions": {"provider": "custom", "model": "m"}}),
            json!({"decisions": {"provider": "custom", "base_url": "https://x.example/d"}}),
        ] {
            assert!(DecisionsConfig::from_config_value(&missing).is_err());
        }
    }

    #[test]
    fn laya_keeps_its_loopback_url_and_refuses_hosted_settings() {
        let c = DecisionsConfig::from_config_value(&json!({"decisions": {"provider": "laya"}}))
            .unwrap();
        assert_eq!(c.provider, DecisionsProvider::laya());
        assert_eq!(c.provider.base_url, "http://127.0.0.1:8791/decide");
        assert_eq!(c.provider.key_env, None);
        for field in [
            ("base_url", "https://openrouter.ai/api/alpha/decisions"),
            ("model", "typesafe/jev-1.13"),
            ("key_env", "OPENROUTER_API_KEY"),
        ] {
            let mut section = json!({"provider": "laya"});
            section[field.0] = json!(field.1);
            let err = DecisionsConfig::from_config_value(&json!({"decisions": section}))
                .unwrap_err()
                .to_string();
            assert!(err.contains("does not apply to provider laya"), "{err}");
        }
    }

    #[test]
    fn invalid_values_fail_loud() {
        assert!(parse_value("decisions.provider", "anthropic").is_err());
        assert!(parse_value("decisions.base_url", "http://openrouter.ai/x").is_err());
        assert!(parse_value("decisions.base_url", "https://u:p@host.example/x").is_err());
        assert!(parse_value("decisions.model", "two words").is_err());
        assert!(parse_value("decisions.key_env", "sk-or-v1-abc").is_err());
        assert!(parse_value("decisions.propose_actions", "maybe").is_err());
        assert!(parse_value("decisions.route_actions.junk", "move:Junk").is_err());
        assert!(parse_value("decisions.route_actions.junk", "delete:x").is_err());
        assert!(
            DecisionsConfig::from_config_value(&json!({"decisions": {"endpoint": "x"}})).is_err()
        );
        assert!(
            DecisionsConfig::from_config_value(
                &json!({"decisions": {"route_actions": {"travel": "add_tag:travel"}}})
            )
            .is_err()
        );
        assert!(
            DecisionsConfig::from_config_value(&json!({"decisions": {"propose_actions": "yes"}}))
                .is_err()
        );
    }

    #[test]
    fn key_catalog() {
        assert!(is_decisions_key("decisions.provider"));
        assert!(is_decisions_key("decisions.route_actions.follow_up"));
        assert!(!is_decisions_key("decisions.route_actions.travel"));
        assert!(!is_decisions_key("decisions.api_key"));
    }

    #[test]
    fn a_flag_picks_that_backend_and_never_falls_back_to_another() {
        let laya = DecisionsConfig::from_config_value(&json!({"decisions": {"provider": "laya"}}))
            .unwrap();
        assert_eq!(laya.provider_for(None).unwrap().backend, JevBackend::Laya);
        assert_eq!(
            laya.provider_for(Some(JevBackend::Openrouter)).unwrap(),
            DecisionsProvider::openrouter()
        );
        let hosted = DecisionsConfig::default();
        assert_eq!(
            hosted.provider_for(Some(JevBackend::Laya)).unwrap(),
            DecisionsProvider::laya()
        );
        assert!(hosted.provider_for(Some(JevBackend::Custom)).is_err());
    }

    #[test]
    fn confident_route_maps_to_a_confirm_offer_and_nothing_else_does() {
        let mut c = DecisionsConfig::default();
        assert_eq!(
            c.offer_for(&policy(MailRoute::FollowUp, false)),
            None,
            "proposals default off"
        );
        c.propose_actions = true;
        let (prompt, then) = c.offer_for(&policy(MailRoute::FollowUp, false)).unwrap();
        assert_eq!(then, vec![ConfirmableAction::AddTag("follow-up".into())]);
        assert_eq!(
            prompt,
            "Jev reads this as follow up. Add the tag follow-up?"
        );

        // apply_policy turned an unconfident route into review + abstained.
        assert_eq!(c.offer_for(&policy(MailRoute::Review, true)), None);
        assert_eq!(c.offer_for(&policy(MailRoute::Review, false)), None);
        assert_eq!(c.offer_for(&policy(MailRoute::Junk, false)), None);
        assert_eq!(c.offer_for(&policy(MailRoute::Routine, false)), None);

        c.route_actions.insert(
            "routine".into(),
            Some(ConfirmableAction::AddTag("travel".into())),
        );
        let (_, then) = c.offer_for(&policy(MailRoute::Routine, false)).unwrap();
        assert_eq!(then, vec![ConfirmableAction::AddTag("travel".into())]);
    }

    #[test]
    fn route_action_labels_round_trip() {
        for raw in ["add_tag:travel", "flag:\\Flagged", "move:Receipts", "none"] {
            let parsed = parse_route_action(raw).unwrap();
            assert_eq!(route_action_label(parsed.as_ref()), raw);
        }
    }
}
