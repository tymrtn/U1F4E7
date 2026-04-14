// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Sieve script generation from Envelope rules.
//!
//! Only rules with pure IMAP-level matches (FROM/TO/SUBJECT) can be
//! exported — tag/score-based rules are local-only and are skipped
//! with a warning. ManageSieve upload is deferred to v0.5.

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

        rule_blocks.push(format!(
            "# {name}\nif {condition} {{\n    {action_str}\n}}",
            name = rule.name,
            condition = condition,
            action_str = action_str,
        ));
    }

    // Build the script
    let mut script = String::new();
    if !requires.is_empty() {
        let mut reqs: Vec<&&str> = requires.iter().collect();
        reqs.sort();
        let req_list = reqs.iter().map(|r| format!("\"{}\"", r)).collect::<Vec<_>>().join(", ");
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
            let parts: Vec<String> = exprs.iter().filter_map(expr_to_sieve).collect();
            if parts.is_empty() {
                return None;
            }
            if parts.len() == 1 {
                return Some(parts.into_iter().next().unwrap());
            }
            Some(format!("allof ({})", parts.join(", ")))
        }
        MatchExpr::Or(exprs) => {
            let parts: Vec<String> = exprs.iter().filter_map(expr_to_sieve).collect();
            if parts.is_empty() {
                return None;
            }
            if parts.len() == 1 {
                return Some(parts.into_iter().next().unwrap());
            }
            Some(format!("anyof ({})", parts.join(", ")))
        }
        MatchExpr::Not(inner) => {
            expr_to_sieve(inner).map(|s| format!("not {s}"))
        }
        // Tags and scores can't be expressed in Sieve
        MatchExpr::HasTag(_) | MatchExpr::ScoreAbove { .. } | MatchExpr::ScoreBelow { .. } => None,
    }
}

fn action_to_sieve<'a>(action: &Action, requires: &mut std::collections::HashSet<&'a str>) -> Option<String> {
    match action {
        Action::Move(folder) => {
            requires.insert("fileinto");
            Some(format!("fileinto \"{folder}\";"))
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
            Some(format!("addflag \"{sieve_flag}\";"))
        }
        Action::Unflag(flag) => {
            requires.insert("imap4flags");
            let lower = flag.to_lowercase();
            let sieve_flag = match lower.as_str() {
                "flagged" => "\\Flagged",
                "seen" => "\\Seen",
                _ => &lower,
            };
            Some(format!("removeflag \"{sieve_flag}\";"))
        }
        Action::Delete => {
            Some("discard;".to_string())
        }
        // Snooze, Unsubscribe, AddTag are local-only
        Action::Snooze(_) | Action::Unsubscribe | Action::AddTag(_) => None,
    }
}

/// Convert glob patterns to Sieve :matches syntax.
/// Sieve uses `*` and `?` the same way as our globs, so minimal conversion needed.
fn glob_to_sieve_match(pattern: &str) -> String {
    // Escape any Sieve-special characters in the pattern (quotes, backslashes)
    pattern.replace('\\', "\\\\").replace('"', "\\\"")
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
        assert!(script.contains("addflag \"\\Flagged\""));
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
}
