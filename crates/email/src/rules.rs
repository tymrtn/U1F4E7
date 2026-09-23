// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Rule matching engine — pure computation, no DB queries.
//!
//! Match expressions and actions are stored as JSON. The CLI builds
//! them from flags (`--match-from`, `--match-tag`, `--match-score-above`).
//! A human-readable DSL may come in v0.5 as syntactic sugar.
//!
//! The engine takes a message's metadata + its tags/scores and returns
//! whether each rule matches. Action execution is handled by the caller
//! (the CLI rule runner or the dashboard).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A match expression tree (stored as JSON in the rules table).
///
/// CLI flags build these via helper constructors; callers never write
/// JSON by hand.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MatchExpr {
    /// Glob match on sender address. `*` matches any sequence, `?` matches one char.
    From(String),
    /// Exact (case-insensitive) match on sender address — no glob interpretation.
    /// Used wherever a sender string must be matched literally: a local-part
    /// containing `*` or `?` (both valid, if unusual, email characters) must
    /// never be reinterpreted as a wildcard.
    FromExact(String),
    /// Glob match on recipient address.
    To(String),
    /// Glob match on subject line.
    Subject(String),
    /// Message has this freeform tag.
    HasTag(String),
    /// Score on `dimension` is above `threshold`.
    ScoreAbove { dimension: String, threshold: f64 },
    /// Score on `dimension` is below `threshold`.
    ScoreBelow { dimension: String, threshold: f64 },
    /// Sender's contact record has this tag.
    ContactHasTag(String),
    /// All sub-expressions must match.
    And(Vec<MatchExpr>),
    /// At least one sub-expression must match.
    Or(Vec<MatchExpr>),
    /// Negation.
    Not(Box<MatchExpr>),
}

impl MatchExpr {
    /// True when any `and`/`or` in the tree has no children. `and []` is
    /// vacuously true and `not (or [])` is too, so such a rule can match
    /// every message; it is refused on save and skipped when rules run.
    pub fn has_empty_condition_list(&self) -> bool {
        match self {
            MatchExpr::And(exprs) | MatchExpr::Or(exprs) => {
                exprs.is_empty() || exprs.iter().any(MatchExpr::has_empty_condition_list)
            }
            MatchExpr::Not(inner) => inner.has_empty_condition_list(),
            _ => false,
        }
    }
}

/// Stable skip reason for a stored rule whose match has an empty condition
/// list. Surfaced in `skipped_rules` and previews, so the wording is part of
/// the contract.
pub const EMPTY_CONDITION_LIST_SKIP_REASON: &str =
    "rule has an empty condition list and could match every message; edit it to add a condition";

/// An action to execute when a rule matches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Move the message to a folder.
    Move(String),
    /// Set an IMAP flag (e.g., "flagged", "seen").
    Flag(String),
    /// Remove an IMAP flag.
    Unflag(String),
    /// Snooze the message with a return time expression.
    Snooze(String),
    /// Permanently delete the message.
    Delete,
    /// Attempt to unsubscribe via List-Unsubscribe header, then move to Junk.
    Unsubscribe,
    /// Add a local tag (enables chained rules).
    AddTag(String),
    /// POST message context as JSON to this URL when the rule matches.
    Webhook(String),
    /// Server-side Sieve `reject` with a reason. Sieve/export-only — the
    /// runtime must never simulate a bounce on already-delivered mail.
    Reject(String),
    /// Server-side Sieve `ereject` with a reason. Preferred over `reject`
    /// where supported because it refuses the message at SMTP time and
    /// avoids generating a backscatter MDN. Sieve/export-only.
    Ereject(String),
    /// Offer a closed, reversible set of actions for a human to confirm.
    /// Executing the rule records an `action_offered` event; `then` runs only
    /// after `envelope actions confirm <event_id>`. Local-only (never Sieve).
    Confirm {
        prompt: String,
        then: Vec<ConfirmableAction>,
    },
}

/// The closed allowlist an offer may carry: reversible, local, no egress.
///
/// Serde rejects every other action shape, and a `move` into Trash or Junk
/// (canonical sentinel or a provider folder name) is rejected at parse time,
/// so an offer can never smuggle a destructive action past a human tap.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", try_from = "RawConfirmableAction")]
pub enum ConfirmableAction {
    AddTag(String),
    Flag(String),
    Move(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum RawConfirmableAction {
    AddTag(String),
    Flag(String),
    Move(String),
}

impl TryFrom<RawConfirmableAction> for ConfirmableAction {
    type Error = String;

    fn try_from(raw: RawConfirmableAction) -> Result<Self, Self::Error> {
        let action = match raw {
            RawConfirmableAction::AddTag(t) => ConfirmableAction::AddTag(t),
            RawConfirmableAction::Flag(f) => ConfirmableAction::Flag(f),
            RawConfirmableAction::Move(m) => ConfirmableAction::Move(m),
        };
        action.validate()?;
        Ok(action)
    }
}

impl ConfirmableAction {
    /// Re-check the allowlist invariants. Serde already ran this at parse;
    /// the executor runs it again before recording or executing an offer.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            ConfirmableAction::AddTag(v) | ConfirmableAction::Flag(v) if v.trim().is_empty() => {
                Err("confirmable action argument must not be empty".to_string())
            }
            ConfirmableAction::Move(dest) if dest.trim().is_empty() => {
                Err("confirmable move destination must not be empty".to_string())
            }
            ConfirmableAction::Move(dest) if is_trash_or_junk_folder(dest) => Err(format!(
                "confirmable move may not target Trash or Junk (got {dest:?})"
            )),
            _ => Ok(()),
        }
    }

    pub fn into_action(self) -> Action {
        match self {
            ConfirmableAction::AddTag(t) => Action::AddTag(t),
            ConfirmableAction::Flag(f) => Action::Flag(f),
            ConfirmableAction::Move(m) => Action::Move(m),
        }
    }

    /// Narrow a plain action into the allowlist, or explain why it cannot be.
    pub fn from_action(action: &Action) -> Result<Self, String> {
        let candidate = match action {
            Action::AddTag(t) => ConfirmableAction::AddTag(t.clone()),
            Action::Flag(f) => ConfirmableAction::Flag(f.clone()),
            Action::Move(m) => ConfirmableAction::Move(m.clone()),
            other => {
                return Err(format!(
                    "action {} is not confirmable; offers allow only add_tag, flag, and move (never to Trash/Junk)",
                    other.kind()
                ));
            }
        };
        candidate.validate()?;
        Ok(candidate)
    }
}

/// True when `dest` names Trash or Junk: a canonical sentinel (`\Trash`,
/// `\Junk`, `\Spam`) or a provider folder the provider map classifies as
/// trash/spam, including hierarchy-prefixed leaves like `INBOX.Trash`.
pub fn is_trash_or_junk_folder(dest: &str) -> bool {
    use crate::provider::canonical;
    let is_bad =
        |kind: Option<&str>| kind == Some(canonical::TRASH) || kind == Some(canonical::SPAM);
    let trimmed = dest.trim();
    if is_bad(crate::folders::canonical_move_key(trimmed))
        || is_bad(crate::provider::classify_folder(trimmed))
    {
        return true;
    }
    let leaf = trimmed.rsplit(['/', '.']).next().unwrap_or(trimmed);
    is_bad(crate::provider::classify_folder(leaf))
}

/// Stable skip reason emitted when a server-side-only action is encountered
/// during local post-delivery rule execution. Surfaced in audit logs and
/// JSON output, so the wording is part of the contract.
pub const SERVER_SIDE_ONLY_SKIP_REASON: &str = "server-side Sieve action; export via 'envelope rule export' and upload to ManageSieve — \
     not executed locally to avoid post-delivery fake bounces";

impl Action {
    /// Stable snake_case kind name (matches the serde tag), used as the
    /// `action_log.action_type` and in human-facing listings.
    pub fn kind(&self) -> &'static str {
        match self {
            Action::Move(_) => "move",
            Action::Flag(_) => "flag",
            Action::Unflag(_) => "unflag",
            Action::Snooze(_) => "snooze",
            Action::Delete => "delete",
            Action::Unsubscribe => "unsubscribe",
            Action::AddTag(_) => "add_tag",
            Action::Webhook(_) => "webhook",
            Action::Reject(_) => "reject",
            Action::Ereject(_) => "ereject",
            Action::Confirm { .. } => "confirm",
        }
    }

    /// Actions that were logged no-ops in batch rule runs before the unified
    /// executor and now mutate mail or local state.
    pub fn became_live_in_unified_executor(&self) -> bool {
        matches!(
            self,
            Action::AddTag(_) | Action::Snooze(_) | Action::Unsubscribe
        )
    }

    /// Whether this action needs `--acknowledge-batch-actions` before a batch
    /// run may execute it (it moves mail or contacts a sender).
    pub fn requires_batch_acknowledgement(&self) -> bool {
        matches!(self, Action::Snooze(_) | Action::Unsubscribe)
    }

    /// Whether this action can only be executed by the mail server via
    /// Sieve. Local rule execution must skip these — Envelope never
    /// fabricates a bounce or MDN for already-delivered mail.
    pub fn is_server_side_only(&self) -> bool {
        matches!(self, Action::Reject(_) | Action::Ereject(_))
    }

    /// Stable skip reason for server-side-only actions, or `None` when the
    /// action can be executed locally.
    pub fn local_execution_skip_reason(&self) -> Option<&'static str> {
        if self.is_server_side_only() {
            Some(SERVER_SIDE_ONLY_SKIP_REASON)
        } else {
            None
        }
    }
}

/// Stable skip reason for a snooze/unsubscribe rule that has not been
/// acknowledged since those actions became live in batch runs. Part of the
/// JSON contract: do not reword.
pub const BATCH_ACTIONS_UNACKNOWLEDGED_REASON: &str = "needs_review: snooze/unsubscribe rule predates unified execution; \
     re-enable with 'envelope rule enable <name> --acknowledge-batch-actions' to let it run";

/// A rule's action as stored in `rules.action`.
///
/// Legacy and ordinary rows are a bare [`Action`] JSON value. Acknowledging a
/// snooze/unsubscribe rule rewrites the row to
/// `{"action": <Action>, "acknowledged_batch_actions": true}`, so the gate
/// needs no schema migration and any rule edit (which writes a bare action)
/// drops the acknowledgement again.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRuleAction {
    pub action: Action,
    pub acknowledged_batch_actions: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcknowledgedEnvelope {
    action: Action,
    acknowledged_batch_actions: bool,
}

impl StoredRuleAction {
    pub fn parse(raw: &str) -> Result<Self, serde_json::Error> {
        match serde_json::from_str::<Action>(raw) {
            Ok(action) => Ok(StoredRuleAction {
                action,
                acknowledged_batch_actions: false,
            }),
            Err(bare_err) => match serde_json::from_str::<AcknowledgedEnvelope>(raw) {
                Ok(env) => Ok(StoredRuleAction {
                    action: env.action,
                    acknowledged_batch_actions: env.acknowledged_batch_actions,
                }),
                Err(_) => Err(bare_err),
            },
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        if self.acknowledged_batch_actions {
            serde_json::to_string(&AcknowledgedEnvelope {
                action: self.action.clone(),
                acknowledged_batch_actions: true,
            })
        } else {
            serde_json::to_string(&self.action)
        }
    }

    /// `Some(reason)` when the compatibility gate must skip this rule.
    pub fn gate_skip_reason(&self) -> Option<&'static str> {
        (self.action.requires_batch_acknowledgement() && !self.acknowledged_batch_actions)
            .then_some(BATCH_ACTIONS_UNACKNOWLEDGED_REASON)
    }
}

/// One step of an authored `confirm` offer: an allowlisted action, or a
/// reference to another rule (by id or name) that is flattened at save time.
#[derive(Deserialize)]
#[serde(untagged)]
enum AuthoredConfirmStep {
    RuleRef { rule: String },
    Action(ConfirmableAction),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoredConfirm {
    prompt: String,
    then: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoredConfirmWrapper {
    confirm: AuthoredConfirm,
}

/// Parse an action as a human authors it, flattening `confirm` rule
/// references into concrete [`ConfirmableAction`]s.
///
/// `resolve_rule` maps a rule id or name to that rule's stored action JSON.
/// The flattened list is what gets stored, so editing the referenced rule
/// later can never change (or escalate) what an existing offer executes.
pub fn parse_authored_action(
    raw: &serde_json::Value,
    resolve_rule: impl Fn(&str) -> Option<String>,
) -> Result<Action, String> {
    let is_confirm = raw.as_object().is_some_and(|o| o.contains_key("confirm"));
    if !is_confirm {
        return serde_json::from_value::<Action>(raw.clone()).map_err(|e| e.to_string());
    }
    let wrapper: AuthoredConfirmWrapper =
        serde_json::from_value(raw.clone()).map_err(|e| format!("invalid confirm action: {e}"))?;
    let AuthoredConfirm { prompt, then } = wrapper.confirm;
    if prompt.trim().is_empty() {
        return Err("confirm prompt must not be empty".to_string());
    }
    let mut flat = Vec::new();
    for step in then {
        let step: AuthoredConfirmStep = serde_json::from_value(step.clone()).map_err(|_| {
            format!(
                "confirm step {step} is not allowed; use add_tag, flag, move (never to Trash/Junk), or {{\"rule\": \"<name or id>\"}}"
            )
        })?;
        match step {
            AuthoredConfirmStep::Action(a) => flat.push(a),
            AuthoredConfirmStep::RuleRef { rule } => {
                let stored = resolve_rule(&rule)
                    .ok_or_else(|| format!("confirm references unknown rule {rule:?}"))?;
                let stored = StoredRuleAction::parse(&stored)
                    .map_err(|e| format!("rule {rule:?} has an invalid action: {e}"))?;
                match stored.action {
                    Action::Confirm { then, .. } => flat.extend(then),
                    other => flat.push(
                        ConfirmableAction::from_action(&other)
                            .map_err(|e| format!("rule {rule:?}: {e}"))?,
                    ),
                }
            }
        }
    }
    if flat.is_empty() {
        return Err("confirm needs at least one action in then".to_string());
    }
    Ok(Action::Confirm { prompt, then: flat })
}

/// Stable score dimension derived from a provider's spam-scoring headers.
///
/// Seeded from `X-Migadu-Spam-Score` (preferred) or `X-Spam-Score` (fallback)
/// during rule test/preview/run. Existing rules match it via `score_above` /
/// `score_below` like any other dimension. The name is part of the rule
/// contract — keep it stable.
pub const PROVIDER_SPAM_DIMENSION: &str = "provider_spam";

/// Derive the `provider_spam` score from raw provider header values.
///
/// Prefers `X-Migadu-Spam-Score`, falling back to `X-Spam-Score`. Each value
/// is parsed as a finite `f64`; a malformed, empty, or non-finite value
/// (`NaN`, `±inf`) is ignored so a garbage header never fabricates a score. A
/// present-but-malformed Migadu header still falls back to a valid generic
/// value.
pub fn provider_spam_from_headers(migadu: Option<&str>, generic: Option<&str>) -> Option<f64> {
    parse_finite_score(migadu).or_else(|| parse_finite_score(generic))
}

fn parse_finite_score(raw: Option<&str>) -> Option<f64> {
    let value: f64 = raw?.trim().parse().ok()?;
    value.is_finite().then_some(value)
}

/// Merge a header-derived `provider_spam` score into a scores map without
/// clobbering an explicitly persisted value.
///
/// An operator-set `provider_spam` score (from `message_scores`) always wins
/// over the derived header signal; the derived value only fills the gap when
/// no persisted score exists for the dimension.
pub fn merge_provider_spam(scores: &mut HashMap<String, f64>, derived: Option<f64>) {
    if let Some(value) = derived {
        scores
            .entry(PROVIDER_SPAM_DIMENSION.to_string())
            .or_insert(value);
    }
}

/// Context about a message for rule evaluation.
///
/// Callers populate this from a `MessageSummary` + the tag/score store.
/// The engine never queries the database itself.
#[derive(Debug)]
pub struct MessageContext {
    pub from_addr: String,
    pub to_addr: String,
    pub subject: String,
    pub tags: Vec<String>,
    pub scores: HashMap<String, f64>,
    pub contact_tags: Vec<String>,
}

/// Evaluate a match expression against a message context.
pub fn evaluate(expr: &MatchExpr, ctx: &MessageContext) -> bool {
    match expr {
        MatchExpr::From(pattern) => glob_match(pattern, &ctx.from_addr),
        MatchExpr::FromExact(addr) => {
            addr.trim().to_lowercase() == ctx.from_addr.trim().to_lowercase()
        }
        MatchExpr::To(pattern) => glob_match(pattern, &ctx.to_addr),
        MatchExpr::Subject(pattern) => glob_match(pattern, &ctx.subject),
        MatchExpr::HasTag(tag) => ctx.tags.iter().any(|t| t == tag),
        MatchExpr::ContactHasTag(tag) => ctx.contact_tags.iter().any(|t| t == tag),
        MatchExpr::ScoreAbove {
            dimension,
            threshold,
        } => ctx.scores.get(dimension).map_or(false, |v| v > threshold),
        MatchExpr::ScoreBelow {
            dimension,
            threshold,
        } => ctx.scores.get(dimension).map_or(false, |v| v < threshold),
        MatchExpr::And(exprs) => exprs.iter().all(|e| evaluate(e, ctx)),
        MatchExpr::Or(exprs) => exprs.iter().any(|e| evaluate(e, ctx)),
        MatchExpr::Not(inner) => !evaluate(inner, ctx),
    }
}

/// Simple glob matching: `*` matches any sequence, `?` matches one char.
/// Case-insensitive.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let text = text.to_lowercase();
    glob_match_inner(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_inner(pattern: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}

/// Build a MatchExpr from CLI-style flags.
///
/// This is the primary way rules are created — the CLI collects
/// `--match-from`, `--match-to`, `--match-subject`, `--match-tag`,
/// `--match-score-above`, `--match-score-below` flags and calls this
/// to produce the JSON-serializable expression.
///
/// Multiple conditions are AND'd together. Returns `None` when no condition
/// was given.
pub fn build_match_expr(
    from: Option<&str>,
    to: Option<&str>,
    subject: Option<&str>,
    tags: &[String],
    score_above: &[(String, f64)],
    score_below: &[(String, f64)],
    contact_tags: &[String],
) -> Option<MatchExpr> {
    let mut conditions: Vec<MatchExpr> = Vec::new();

    if let Some(f) = from {
        conditions.push(MatchExpr::From(f.to_string()));
    }
    if let Some(t) = to {
        conditions.push(MatchExpr::To(t.to_string()));
    }
    if let Some(s) = subject {
        conditions.push(MatchExpr::Subject(s.to_string()));
    }
    for tag in tags {
        conditions.push(MatchExpr::HasTag(tag.clone()));
    }
    for (dim, val) in score_above {
        conditions.push(MatchExpr::ScoreAbove {
            dimension: dim.clone(),
            threshold: *val,
        });
    }
    for (dim, val) in score_below {
        conditions.push(MatchExpr::ScoreBelow {
            dimension: dim.clone(),
            threshold: *val,
        });
    }
    for ct in contact_tags {
        conditions.push(MatchExpr::ContactHasTag(ct.clone()));
    }

    match conditions.len() {
        0 => None,
        1 => Some(conditions.remove(0)),
        _ => Some(MatchExpr::And(conditions)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(from: &str, to: &str, subject: &str) -> MessageContext {
        MessageContext {
            from_addr: from.to_string(),
            to_addr: to.to_string(),
            subject: subject.to_string(),
            tags: vec![],
            scores: HashMap::new(),
            contact_tags: vec![],
        }
    }

    fn ctx_with_tags(from: &str, tags: &[&str], scores: &[(&str, f64)]) -> MessageContext {
        MessageContext {
            from_addr: from.to_string(),
            to_addr: String::new(),
            subject: String::new(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            scores: scores.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            contact_tags: vec![],
        }
    }

    // ── Glob matching ───────────────────────────────────────────────

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("alice@example.com", "alice@example.com"));
        assert!(!glob_match("alice@example.com", "bob@example.com"));
    }

    #[test]
    fn glob_star_wildcard() {
        assert!(glob_match(
            "*@notifications.github.com",
            "noreply@notifications.github.com"
        ));
        assert!(glob_match(
            "*@*.github.com",
            "noreply@notifications.github.com"
        ));
        assert!(!glob_match(
            "*@github.com",
            "noreply@notifications.github.com"
        ));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
    }

    #[test]
    fn glob_case_insensitive() {
        assert!(glob_match("*@GITHUB.COM", "noreply@github.com"));
    }

    // ── MatchExpr evaluation ────────────────────────────────────────

    #[test]
    fn match_from() {
        let expr = MatchExpr::From("*@notifications.github.com".to_string());
        assert!(evaluate(
            &expr,
            &ctx("noreply@notifications.github.com", "", "")
        ));
        assert!(!evaluate(&expr, &ctx("alice@example.com", "", "")));
    }

    #[test]
    fn match_from_exact_does_not_treat_glob_chars_as_wildcards() {
        // A literal local-part of "*" must match only that exact address —
        // never broaden into every sender at the domain.
        let expr = MatchExpr::FromExact("*@example.com".to_string());
        assert!(evaluate(&expr, &ctx("*@example.com", "", "")));
        assert!(!evaluate(&expr, &ctx("anything@example.com", "", "")));
        assert!(!evaluate(&expr, &ctx("evil@example.com", "", "")));
    }

    #[test]
    fn match_from_exact_is_case_insensitive_and_trims() {
        let expr = MatchExpr::FromExact("Alice@Example.com".to_string());
        assert!(evaluate(&expr, &ctx(" alice@example.com ", "", "")));
    }

    #[test]
    fn match_subject() {
        let expr = MatchExpr::Subject("*invoice*".to_string());
        assert!(evaluate(&expr, &ctx("", "", "Your invoice for March")));
        assert!(!evaluate(&expr, &ctx("", "", "Hello world")));
    }

    #[test]
    fn match_has_tag() {
        let expr = MatchExpr::HasTag("newsletter".to_string());
        assert!(evaluate(
            &expr,
            &ctx_with_tags("", &["newsletter", "automated"], &[])
        ));
        assert!(!evaluate(&expr, &ctx_with_tags("", &["vip"], &[])));
    }

    #[test]
    fn match_score_above() {
        let expr = MatchExpr::ScoreAbove {
            dimension: "urgent".to_string(),
            threshold: 0.7,
        };
        assert!(evaluate(&expr, &ctx_with_tags("", &[], &[("urgent", 0.9)])));
        assert!(!evaluate(
            &expr,
            &ctx_with_tags("", &[], &[("urgent", 0.5)])
        ));
        assert!(!evaluate(&expr, &ctx_with_tags("", &[], &[]))); // no score = no match
    }

    #[test]
    fn match_and() {
        let expr = MatchExpr::And(vec![
            MatchExpr::HasTag("newsletter".to_string()),
            MatchExpr::ScoreBelow {
                dimension: "interesting".to_string(),
                threshold: 0.3,
            },
        ]);
        assert!(evaluate(
            &expr,
            &ctx_with_tags("", &["newsletter"], &[("interesting", 0.1)])
        ));
        assert!(!evaluate(
            &expr,
            &ctx_with_tags("", &["newsletter"], &[("interesting", 0.5)])
        ));
        assert!(!evaluate(
            &expr,
            &ctx_with_tags("", &[], &[("interesting", 0.1)])
        ));
    }

    #[test]
    fn match_or() {
        let expr = MatchExpr::Or(vec![
            MatchExpr::From("*@spam.com".to_string()),
            MatchExpr::HasTag("junk".to_string()),
        ]);
        assert!(evaluate(&expr, &ctx("noreply@spam.com", "", "")));
        assert!(evaluate(
            &expr,
            &ctx_with_tags("alice@example.com", &["junk"], &[])
        ));
        assert!(!evaluate(&expr, &ctx("alice@example.com", "", "")));
    }

    #[test]
    fn match_not() {
        let expr = MatchExpr::Not(Box::new(MatchExpr::HasTag("important".to_string())));
        assert!(evaluate(&expr, &ctx_with_tags("", &[], &[])));
        assert!(!evaluate(&expr, &ctx_with_tags("", &["important"], &[])));
    }

    // ── Empty condition lists ───────────────────────────────────────

    #[test]
    fn empty_condition_list_is_detected_anywhere_in_the_tree() {
        let from = || MatchExpr::From("*@x.example".to_string());
        for expr in [
            MatchExpr::And(vec![]),
            MatchExpr::Or(vec![]),
            MatchExpr::Or(vec![from(), MatchExpr::And(vec![])]),
            MatchExpr::Not(Box::new(MatchExpr::Or(vec![]))),
        ] {
            assert!(expr.has_empty_condition_list(), "{expr:?}");
        }
    }

    #[test]
    fn non_empty_expressions_have_no_empty_condition_list() {
        for expr in [
            MatchExpr::From("*@x.example".to_string()),
            MatchExpr::And(vec![MatchExpr::HasTag("a".to_string())]),
            MatchExpr::Or(vec![
                MatchExpr::Subject("*invoice*".to_string()),
                MatchExpr::Not(Box::new(MatchExpr::And(vec![MatchExpr::To(
                    "me@x.example".to_string(),
                )]))),
            ]),
        ] {
            assert!(!expr.has_empty_condition_list(), "{expr:?}");
        }
    }

    // ── JSON serialization round-trip ───────────────────────────────

    #[test]
    fn match_expr_json_roundtrip() {
        let expr = MatchExpr::And(vec![
            MatchExpr::From("*@github.com".to_string()),
            MatchExpr::ScoreAbove {
                dimension: "urgent".to_string(),
                threshold: 0.7,
            },
        ]);
        let json = serde_json::to_string(&expr).unwrap();
        let parsed: MatchExpr = serde_json::from_str(&json).unwrap();
        assert_eq!(expr, parsed);
    }

    #[test]
    fn from_exact_json_roundtrip() {
        let expr = MatchExpr::FromExact("*@example.com".to_string());
        let json = serde_json::to_string(&expr).unwrap();
        assert_eq!(json, r#"{"from_exact":"*@example.com"}"#);
        let parsed: MatchExpr = serde_json::from_str(&json).unwrap();
        assert_eq!(expr, parsed);
    }

    #[test]
    fn action_json_roundtrip() {
        let actions = vec![
            Action::Move("Junk".to_string()),
            Action::Flag("flagged".to_string()),
            Action::Delete,
            Action::Unsubscribe,
            Action::AddTag("processed".to_string()),
            Action::Reject("Address closed".to_string()),
            Action::Ereject("Address closed".to_string()),
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            let parsed: Action = serde_json::from_str(&json).unwrap();
            assert_eq!(action, parsed);
        }
    }

    #[test]
    fn reject_action_serializes_with_reason() {
        let a = Action::Reject("Mailbox no longer monitored".to_string());
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, r#"{"reject":"Mailbox no longer monitored"}"#);
    }

    #[test]
    fn ereject_action_serializes_with_reason() {
        let a = Action::Ereject("Mailbox no longer monitored".to_string());
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, r#"{"ereject":"Mailbox no longer monitored"}"#);
    }

    #[test]
    fn reject_and_ereject_are_server_side_only() {
        assert!(Action::Reject("x".to_string()).is_server_side_only());
        assert!(Action::Ereject("x".to_string()).is_server_side_only());
    }

    #[test]
    fn other_actions_are_not_server_side_only() {
        assert!(!Action::Move("Junk".to_string()).is_server_side_only());
        assert!(!Action::Flag("flagged".to_string()).is_server_side_only());
        assert!(!Action::Delete.is_server_side_only());
        assert!(!Action::Unsubscribe.is_server_side_only());
        assert!(!Action::AddTag("t".to_string()).is_server_side_only());
        assert!(!Action::Webhook("https://example.test".to_string()).is_server_side_only());
        assert!(!Action::Snooze("1d".to_string()).is_server_side_only());
        assert!(!Action::Unflag("seen".to_string()).is_server_side_only());
    }

    #[test]
    fn server_side_only_skip_reason_is_stable() {
        let reason = Action::Reject("x".to_string())
            .local_execution_skip_reason()
            .expect("reject must have a skip reason");
        assert!(
            reason.contains("server-side") && reason.contains("Sieve"),
            "skip reason must say it's server-side Sieve: {reason}"
        );
        // Same stable string for ereject.
        assert_eq!(
            reason,
            Action::Ereject("y".to_string())
                .local_execution_skip_reason()
                .unwrap()
        );
    }

    #[test]
    fn non_server_side_actions_have_no_skip_reason() {
        assert!(
            Action::Move("Junk".to_string())
                .local_execution_skip_reason()
                .is_none()
        );
        assert!(Action::Delete.local_execution_skip_reason().is_none());
    }

    // ── provider_spam score derivation ──────────────────────────────

    #[test]
    fn provider_spam_prefers_migadu_header() {
        assert_eq!(
            provider_spam_from_headers(Some("3.5"), None),
            Some(3.5),
            "Migadu header alone should seed the score"
        );
    }

    #[test]
    fn provider_spam_falls_back_to_generic_header() {
        assert_eq!(
            provider_spam_from_headers(None, Some("5.1")),
            Some(5.1),
            "generic X-Spam-Score should be used when Migadu is absent"
        );
    }

    #[test]
    fn provider_spam_migadu_wins_over_generic() {
        assert_eq!(
            provider_spam_from_headers(Some("2.0"), Some("9.9")),
            Some(2.0),
            "Migadu must take precedence when both headers are present"
        );
    }

    #[test]
    fn provider_spam_malformed_migadu_falls_back_to_generic() {
        assert_eq!(
            provider_spam_from_headers(Some("not-a-number"), Some("4.2")),
            Some(4.2),
            "a malformed Migadu value must not block the generic fallback"
        );
    }

    #[test]
    fn provider_spam_ignores_malformed_and_non_finite() {
        assert_eq!(
            provider_spam_from_headers(Some("bogus"), Some("junk")),
            None
        );
        assert_eq!(provider_spam_from_headers(Some("NaN"), None), None);
        assert_eq!(provider_spam_from_headers(Some("inf"), None), None);
        assert_eq!(provider_spam_from_headers(Some("-inf"), None), None);
        assert_eq!(provider_spam_from_headers(None, None), None);
        assert_eq!(provider_spam_from_headers(Some("  "), None), None);
    }

    #[test]
    fn provider_spam_parses_negative_and_padded_values() {
        assert_eq!(provider_spam_from_headers(Some(" -2.6 "), None), Some(-2.6));
    }

    #[test]
    fn merge_provider_spam_seeds_when_absent() {
        let mut scores: HashMap<String, f64> = HashMap::new();
        merge_provider_spam(&mut scores, Some(3.5));
        assert_eq!(scores.get(PROVIDER_SPAM_DIMENSION), Some(&3.5));
    }

    #[test]
    fn merge_provider_spam_does_not_clobber_persisted() {
        let mut scores: HashMap<String, f64> = HashMap::new();
        scores.insert(PROVIDER_SPAM_DIMENSION.to_string(), 8.0);
        merge_provider_spam(&mut scores, Some(3.5));
        assert_eq!(
            scores.get(PROVIDER_SPAM_DIMENSION),
            Some(&8.0),
            "an explicitly persisted provider_spam score must win"
        );
    }

    #[test]
    fn merge_provider_spam_none_is_noop() {
        let mut scores: HashMap<String, f64> = HashMap::new();
        merge_provider_spam(&mut scores, None);
        assert!(scores.is_empty());
    }

    // ── build_match_expr helper ─────────────────────────────────────

    #[test]
    fn build_from_cli_flags() {
        let expr = build_match_expr(
            Some("*@github.com"),
            None,
            None,
            &["automated".to_string()],
            &[("urgent".to_string(), 0.7)],
            &[],
            &[],
        );
        // Should be And([From, HasTag, ScoreAbove])
        if let Some(MatchExpr::And(conditions)) = &expr {
            assert_eq!(conditions.len(), 3);
        } else {
            panic!("expected And, got {expr:?}");
        }
    }

    #[test]
    fn build_single_condition_unwraps() {
        let expr = build_match_expr(Some("*@github.com"), None, None, &[], &[], &[], &[]);
        assert!(matches!(expr, Some(MatchExpr::From(_))));
    }

    #[test]
    fn build_with_no_conditions_is_none() {
        assert_eq!(build_match_expr(None, None, None, &[], &[], &[], &[]), None);
    }

    // ── Confirm offers ──────────────────────────────────────────────

    #[test]
    fn confirm_json_roundtrips_with_allowlisted_actions() {
        let action = Action::Confirm {
            prompt: "Looks like a trip".to_string(),
            then: vec![
                ConfirmableAction::AddTag("travel".to_string()),
                ConfirmableAction::Flag("flagged".to_string()),
                ConfirmableAction::Move("Travel".to_string()),
            ],
        };
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(
            json,
            r#"{"confirm":{"prompt":"Looks like a trip","then":[{"add_tag":"travel"},{"flag":"flagged"},{"move":"Travel"}]}}"#
        );
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
    }

    #[test]
    fn confirm_json_rejects_delete_webhook_and_other_actions() {
        for step in [
            r#""delete""#,
            r#"{"webhook":"https://attacker.example/x"}"#,
            r#""unsubscribe""#,
            r#"{"snooze":"1d"}"#,
            r#"{"unflag":"seen"}"#,
            r#"{"reject":"no"}"#,
            r#"{"confirm":{"prompt":"x","then":[]}}"#,
        ] {
            let json = format!(r#"{{"confirm":{{"prompt":"p","then":[{step}]}}}}"#);
            assert!(
                serde_json::from_str::<Action>(&json).is_err(),
                "confirm must reject {step}"
            );
        }
    }

    #[test]
    fn confirm_json_rejects_move_to_trash_or_junk() {
        for dest in [
            "\\Trash",
            "\\Junk",
            "\\Spam",
            "Trash",
            "[Gmail]/Trash",
            "[Gmail]/Spam",
            "Junk",
            "Deleted Items",
            "INBOX.Trash",
            "INBOX/Junk",
        ] {
            let json = serde_json::json!({"confirm": {"prompt": "p", "then": [{"move": dest}]}});
            assert!(
                serde_json::from_value::<Action>(json).is_err(),
                "move to {dest} must be rejected"
            );
        }
        let ok = serde_json::json!({"confirm": {"prompt": "p", "then": [{"move": "\\Archive"}]}});
        assert!(serde_json::from_value::<Action>(ok).is_ok());
    }

    #[test]
    fn authored_confirm_flattens_rule_reference_at_save_time() {
        let authored = serde_json::json!({"confirm": {
            "prompt": "Trip?",
            "then": [{"flag": "flagged"}, {"rule": "travel-tag"}]
        }});
        let action = parse_authored_action(&authored, |r| {
            (r == "travel-tag").then(|| r#"{"add_tag":"travel"}"#.to_string())
        })
        .unwrap();
        assert_eq!(
            action,
            Action::Confirm {
                prompt: "Trip?".to_string(),
                then: vec![
                    ConfirmableAction::Flag("flagged".to_string()),
                    ConfirmableAction::AddTag("travel".to_string()),
                ],
            }
        );
        // The stored JSON carries no rule reference to dereference later.
        assert!(!serde_json::to_string(&action).unwrap().contains("rule"));
    }

    #[test]
    fn authored_confirm_rejects_reference_to_non_confirmable_rule() {
        let authored = serde_json::json!({"confirm": {"prompt": "p", "then": [{"rule": "nuke"}]}});
        for stored in [
            r#""delete""#,
            r#"{"webhook":"https://x.example"}"#,
            r#"{"move":"Trash"}"#,
        ] {
            let err = parse_authored_action(&authored, |_| Some(stored.to_string())).unwrap_err();
            assert!(err.contains("nuke"), "{err}");
        }
        let err = parse_authored_action(&authored, |_| None).unwrap_err();
        assert!(err.contains("unknown rule"), "{err}");
    }

    #[test]
    fn authored_plain_action_passes_through() {
        let action =
            parse_authored_action(&serde_json::json!({"move": "Archive"}), |_| None).unwrap();
        assert_eq!(action, Action::Move("Archive".to_string()));
    }

    #[test]
    fn confirm_is_local_only_not_server_side() {
        let a = Action::Confirm {
            prompt: "p".to_string(),
            then: vec![ConfirmableAction::AddTag("t".to_string())],
        };
        assert!(!a.is_server_side_only());
        assert_eq!(a.kind(), "confirm");
    }

    // ── Stored action + compatibility gate ──────────────────────────

    #[test]
    fn stored_action_parses_bare_legacy_json() {
        let stored = StoredRuleAction::parse(r#"{"snooze":"1d"}"#).unwrap();
        assert_eq!(stored.action, Action::Snooze("1d".to_string()));
        assert!(!stored.acknowledged_batch_actions);
        assert_eq!(
            stored.gate_skip_reason(),
            Some(BATCH_ACTIONS_UNACKNOWLEDGED_REASON)
        );
        assert_eq!(
            StoredRuleAction::parse(r#""unsubscribe""#)
                .unwrap()
                .gate_skip_reason(),
            Some(BATCH_ACTIONS_UNACKNOWLEDGED_REASON)
        );
    }

    #[test]
    fn acknowledged_stored_action_roundtrips_and_passes_gate() {
        let stored = StoredRuleAction {
            action: Action::Unsubscribe,
            acknowledged_batch_actions: true,
        };
        let json = stored.to_json().unwrap();
        assert_eq!(
            json,
            r#"{"action":"unsubscribe","acknowledged_batch_actions":true}"#
        );
        let parsed = StoredRuleAction::parse(&json).unwrap();
        assert_eq!(parsed, stored);
        assert_eq!(parsed.gate_skip_reason(), None);
    }

    #[test]
    fn gate_ignores_actions_that_do_not_need_acknowledgement() {
        for raw in [r#"{"add_tag":"t"}"#, r#"{"move":"Archive"}"#, r#""delete""#] {
            assert_eq!(
                StoredRuleAction::parse(raw).unwrap().gate_skip_reason(),
                None
            );
        }
    }

    #[test]
    fn gate_reason_is_stable() {
        assert!(BATCH_ACTIONS_UNACKNOWLEDGED_REASON.starts_with("needs_review:"));
        assert!(BATCH_ACTIONS_UNACKNOWLEDGED_REASON.contains("--acknowledge-batch-actions"));
    }
}
