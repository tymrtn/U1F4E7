// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Sieve script generation from Envelope rules.
//!
//! Only rules with pure IMAP-level matches (FROM/TO/SUBJECT) can be
//! exported — tag/score-based rules are local-only and are skipped
//! with a warning.
//!
//! Server-side `reject` / `ereject` actions are export-only: Envelope
//! emits them here so an operator can upload the script via ManageSieve
//! (or paste it into the provider UI). Live ManageSieve publish from
//! within Envelope is tracked separately and is not part of this slice.
//! Local post-delivery rule execution must never fabricate a bounce —
//! [`Action::is_server_side_only`](crate::rules::Action::is_server_side_only)
//! gates those paths.

use envelope_email_store::models::Rule;

use crate::rules::{Action, MatchExpr};

/// Export a set of rules as a Sieve script string.
///
/// Rules whose `sieve_exportable` flag is false are skipped. Returns
/// the script text and a list of skipped rule names.
pub fn export_sieve(rules: &[Rule]) -> (String, Vec<String>) {
    let mut requires: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut rule_blocks: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for rule in rules {
        if !rule.enabled {
            continue;
        }
        if !rule.sieve_exportable {
            skipped.push(rule.name.clone());
            continue;
        }

        let match_expr: MatchExpr = match serde_json::from_str(&rule.match_expr) {
            Ok(e) => e,
            Err(_) => {
                skipped.push(rule.name.clone());
                continue;
            }
        };

        let action: Action = match serde_json::from_str(&rule.action) {
            Ok(a) => a,
            Err(_) => {
                skipped.push(rule.name.clone());
                continue;
            }
        };

        let condition = match expr_to_sieve(&match_expr) {
            Some(c) => c,
            None => {
                skipped.push(rule.name.clone());
                continue;
            }
        };

        let action_str = match action_to_sieve(&action, &mut requires) {
            Some(a) => a,
            None => {
                skipped.push(rule.name.clone());
                continue;
            }
        };
        let stop_str = if rule.stop
            || matches!(
                action,
                Action::Move(_) | Action::Delete | Action::Reject(_) | Action::Ereject(_)
            ) {
            "\n    stop;"
        } else {
            ""
        };

        rule_blocks.push(format!(
            "# {name}\nif {condition} {{\n    {action_str}{stop_str}\n}}",
            name = sieve_comment(&rule.name),
            condition = condition,
            action_str = action_str,
        ));
    }

    // Build the script
    let mut script = String::new();
    if !requires.is_empty() {
        let mut reqs: Vec<&&str> = requires.iter().collect();
        reqs.sort();
        let req_list = reqs
            .iter()
            .map(|r| format!("\"{}\"", r))
            .collect::<Vec<_>>()
            .join(", ");
        script.push_str(&format!("require [{req_list}];\n\n"));
    }

    for (i, block) in rule_blocks.iter().enumerate() {
        script.push_str(block);
        if i < rule_blocks.len() - 1 {
            script.push_str("\n\n");
        }
        script.push('\n');
    }

    (script, skipped)
}

fn expr_to_sieve(expr: &MatchExpr) -> Option<String> {
    match expr {
        MatchExpr::From(pattern) => {
            let addr = glob_to_sieve_match(pattern);
            Some(format!("address :matches \"from\" \"{addr}\""))
        }
        MatchExpr::To(pattern) => {
            let addr = glob_to_sieve_match(pattern);
            Some(format!("address :matches \"to\" \"{addr}\""))
        }
        MatchExpr::Subject(pattern) => {
            let subj = glob_to_sieve_match(pattern);
            Some(format!("header :matches \"subject\" \"{subj}\""))
        }
        MatchExpr::And(exprs) => {
            let parts: Option<Vec<String>> = exprs.iter().map(expr_to_sieve).collect();
            let parts = parts?;
            if parts.is_empty() {
                return None;
            }
            if parts.len() == 1 {
                return Some(parts.into_iter().next().unwrap());
            }
            Some(format!("allof ({})", parts.join(", ")))
        }
        MatchExpr::Or(exprs) => {
            let parts: Option<Vec<String>> = exprs.iter().map(expr_to_sieve).collect();
            let parts = parts?;
            if parts.is_empty() {
                return None;
            }
            if parts.len() == 1 {
                return Some(parts.into_iter().next().unwrap());
            }
            Some(format!("anyof ({})", parts.join(", ")))
        }
        MatchExpr::Not(inner) => expr_to_sieve(inner).map(|s| format!("not {s}")),
        // Tags, scores, and contact tags can't be expressed in Sieve
        MatchExpr::HasTag(_)
        | MatchExpr::ScoreAbove { .. }
        | MatchExpr::ScoreBelow { .. }
        | MatchExpr::ContactHasTag(_) => None,
    }
}

fn action_to_sieve<'a>(
    action: &Action,
    requires: &mut std::collections::HashSet<&'a str>,
) -> Option<String> {
    match action {
        Action::Move(folder) => {
            requires.insert("fileinto");
            Some(format!("fileinto {};", sieve_string(folder)))
        }
        Action::Flag(flag) => {
            requires.insert("imap4flags");
            let lower = flag.to_lowercase();
            let sieve_flag = match lower.as_str() {
                "flagged" => "\\Flagged",
                "seen" => "\\Seen",
                "answered" => "\\Answered",
                "draft" => "\\Draft",
                "deleted" => "\\Deleted",
                _ => &lower,
            };
            Some(format!("addflag {};", sieve_string(sieve_flag)))
        }
        Action::Unflag(flag) => {
            requires.insert("imap4flags");
            let lower = flag.to_lowercase();
            let sieve_flag = match lower.as_str() {
                "flagged" => "\\Flagged",
                "seen" => "\\Seen",
                _ => &lower,
            };
            Some(format!("removeflag {};", sieve_string(sieve_flag)))
        }
        Action::Delete => Some("discard;".to_string()),
        Action::Reject(reason) => {
            requires.insert("reject");
            Some(format!(
                "reject {};",
                sieve_string(&sanitize_reason(reason))
            ))
        }
        Action::Ereject(reason) => {
            requires.insert("ereject");
            Some(format!(
                "ereject {};",
                sieve_string(&sanitize_reason(reason))
            ))
        }
        // Snooze, Unsubscribe, AddTag, Webhook are local-only
        Action::Snooze(_) | Action::Unsubscribe | Action::AddTag(_) | Action::Webhook(_) => None,
    }
}

/// Collapse newlines/carriage returns and trim whitespace from a reject/ereject
/// reason before it is wrapped in a Sieve quoted string. RFC 5228 quoted
/// strings do not allow raw line breaks; flattening keeps the emitted script
/// parseable without silently dropping operator-supplied text.
fn sanitize_reason(reason: &str) -> String {
    reason.replace(['\r', '\n'], " ")
}

/// Convert glob patterns to Sieve :matches syntax.
/// Sieve uses `*` and `?` the same way as our globs, so minimal conversion needed.
fn glob_to_sieve_match(pattern: &str) -> String {
    // Escape any Sieve-special characters in the pattern (quotes, backslashes)
    pattern.replace('\\', "\\\\").replace('"', "\\\"")
}

fn sieve_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn sieve_comment(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rule(name: &str, match_json: &str, action_json: &str, exportable: bool) -> Rule {
        Rule {
            id: "test-id".to_string(),
            account_id: "test".to_string(),
            name: name.to_string(),
            match_expr: match_json.to_string(),
            action: action_json.to_string(),
            enabled: true,
            priority: 100,
            stop: false,
            sieve_exportable: exportable,
            hit_count: 0,
            last_hit_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn export_from_fileinto() {
        let rules = vec![make_rule(
            "GitHub noise",
            r#"{"from":"*@notifications.github.com"}"#,
            r#"{"move":"Archive"}"#,
            true,
        )];
        let (script, skipped) = export_sieve(&rules);
        assert!(skipped.is_empty());
        assert!(script.contains("require [\"fileinto\"]"));
        assert!(script.contains("address :matches \"from\" \"*@notifications.github.com\""));
        assert!(script.contains("fileinto \"Archive\""));
    }

    #[test]
    fn export_flag() {
        let rules = vec![make_rule(
            "Star important",
            r#"{"subject":"*urgent*"}"#,
            r#"{"flag":"flagged"}"#,
            true,
        )];
        let (script, _) = export_sieve(&rules);
        assert!(script.contains("imap4flags"));
        assert!(script.contains("addflag \"\\\\Flagged\""));
    }

    #[test]
    fn skip_tag_based_rules() {
        let rules = vec![make_rule(
            "Tag-based",
            r#"{"has_tag":"newsletter"}"#,
            r#"{"move":"Junk"}"#,
            false,
        )];
        let (script, skipped) = export_sieve(&rules);
        assert_eq!(skipped, vec!["Tag-based"]);
        assert!(script.is_empty() || !script.contains("Tag-based"));
    }

    #[test]
    fn export_and_condition() {
        let rules = vec![make_rule(
            "Compound",
            r#"{"and":[{"from":"*@spam.com"},{"subject":"*offer*"}]}"#,
            r#""delete""#,
            true,
        )];
        let (script, _) = export_sieve(&rules);
        assert!(script.contains("allof"));
        assert!(script.contains("discard;"));
    }

    #[test]
    fn export_multiple_rules() {
        let rules = vec![
            make_rule("Rule 1", r#"{"from":"*@a.com"}"#, r#"{"move":"A"}"#, true),
            make_rule("Rule 2", r#"{"from":"*@b.com"}"#, r#"{"move":"B"}"#, true),
        ];
        let (script, skipped) = export_sieve(&rules);
        assert!(skipped.is_empty());
        assert!(script.contains("# Rule 1"));
        assert!(script.contains("# Rule 2"));
    }

    #[test]
    fn mixed_local_and_server_predicate_is_not_partially_exported() {
        let rules = vec![make_rule(
            "Newsletter guard",
            r#"{"and":[{"from":"*@news.example"},{"has_tag":"newsletter"}]}"#,
            r#"{"move":"Archive"}"#,
            true,
        )];
        let (script, skipped) = export_sieve(&rules);
        assert_eq!(skipped, vec!["Newsletter guard"]);
        assert!(!script.contains("news.example"));
    }

    #[test]
    fn export_reject_emits_require_action_and_stop() {
        let rules = vec![make_rule(
            "Closed address",
            r#"{"from":"*@bounce.example"}"#,
            r#"{"reject":"This mailbox is closed."}"#,
            true,
        )];
        let (script, skipped) = export_sieve(&rules);
        assert!(skipped.is_empty(), "expected no skipped rules: {skipped:?}");
        assert!(
            script.contains("require [\"reject\"]"),
            "missing require line: {script}"
        );
        assert!(
            script.contains(r#"reject "This mailbox is closed.";"#),
            "missing reject action: {script}"
        );
        assert!(
            script.contains("\n    stop;"),
            "reject must be followed by stop;: {script}"
        );
    }

    #[test]
    fn export_ereject_emits_require_action_and_stop() {
        let rules = vec![make_rule(
            "Drop completely",
            r#"{"from":"*@spam.example"}"#,
            r#"{"ereject":"Mailbox unreachable."}"#,
            true,
        )];
        let (script, skipped) = export_sieve(&rules);
        assert!(skipped.is_empty(), "expected no skipped rules: {skipped:?}");
        assert!(
            script.contains("require [\"ereject\"]"),
            "missing require line: {script}"
        );
        assert!(
            script.contains(r#"ereject "Mailbox unreachable.";"#),
            "missing ereject action: {script}"
        );
        assert!(
            script.contains("\n    stop;"),
            "ereject must be followed by stop;: {script}"
        );
    }

    #[test]
    fn export_reject_escapes_quotes_and_backslashes() {
        let rules = vec![make_rule(
            "Quoted reject",
            r#"{"from":"*@a.example"}"#,
            // Reason contains a backslash and a double-quote that must be escaped
            // in the emitted Sieve quoted string.
            r#"{"reject":"Say \"no\" \\stop"}"#,
            true,
        )];
        let (script, skipped) = export_sieve(&rules);
        assert!(skipped.is_empty(), "expected no skipped rules: {skipped:?}");
        // Sieve quoted strings escape \ as \\ and " as \".
        assert!(
            script.contains(r#"reject "Say \"no\" \\stop";"#),
            "reject reason not safely escaped: {script}"
        );
    }

    #[test]
    fn export_reject_normalizes_newlines_in_reason() {
        // Embedded newlines must not break the Sieve quoted string. They are
        // normalized to spaces before emission so the script stays parseable.
        let rules = vec![make_rule(
            "Multiline reject",
            r#"{"from":"*@a.example"}"#,
            r#"{"reject":"Line one\nLine two"}"#,
            true,
        )];
        let (script, skipped) = export_sieve(&rules);
        assert!(skipped.is_empty(), "expected no skipped rules: {skipped:?}");
        // The reject string itself must be on a single line.
        let action_line = script
            .lines()
            .find(|l| l.contains("reject "))
            .expect("reject line present");
        assert!(
            !action_line.contains("\n"),
            "action line has raw newline: {action_line:?}"
        );
        assert!(action_line.contains("Line one"));
        assert!(action_line.contains("Line two"));
    }

    #[test]
    fn export_escapes_sieve_strings_and_comments() {
        let rules = vec![make_rule(
            "Bad\n# injected",
            r#"{"from":"*@example.com"}"#,
            r#"{"move":"Archive\"; discard; #"}"#,
            true,
        )];
        let (script, skipped) = export_sieve(&rules);
        assert!(skipped.is_empty());
        assert!(script.contains("# Bad # injected"));
        assert!(script.contains(r#"fileinto "Archive\"; discard; #";"#));
        assert!(!script.contains(
            "
# injected"
        ));
    }
}
