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
/// Multiple conditions are AND'd together.
pub fn build_match_expr(
    from: Option<&str>,
    to: Option<&str>,
    subject: Option<&str>,
    tags: &[String],
    score_above: &[(String, f64)],
    score_below: &[(String, f64)],
    contact_tags: &[String],
) -> MatchExpr {
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
        0 => MatchExpr::And(vec![]), // matches nothing
        1 => conditions.remove(0),
        _ => MatchExpr::And(conditions),
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
    fn action_json_roundtrip() {
        let actions = vec![
            Action::Move("Junk".to_string()),
            Action::Flag("flagged".to_string()),
            Action::Delete,
            Action::Unsubscribe,
            Action::AddTag("processed".to_string()),
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            let parsed: Action = serde_json::from_str(&json).unwrap();
            assert_eq!(action, parsed);
        }
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
        if let MatchExpr::And(conditions) = &expr {
            assert_eq!(conditions.len(), 3);
        } else {
            panic!("expected And, got {expr:?}");
        }
    }

    #[test]
    fn build_single_condition_unwraps() {
        let expr = build_match_expr(Some("*@github.com"), None, None, &[], &[], &[], &[]);
        assert!(matches!(expr, MatchExpr::From(_)));
    }
}
