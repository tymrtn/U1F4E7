// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! The agent-authored body of an outbound message, checked for encoding damage
//! on the way in.
//!
//! Every surface that accepts an authored body — CLI `draft create/reply/
//! forward/edit`, `send`, and the MCP tools behind them — constructs one of
//! these instead of passing raw `&str` around. That is deliberate: the type is
//! the only way to reach the draft-building functions, so a new surface cannot
//! quietly skip the check.
//!
//! What it catches: a body whose line breaks arrived as the two characters `\`
//! and `n` (shell quoting, or a double-encoded JSON string) instead of real line
//! breaks. See [`envelope_email_transport::escapes`] for the exact rule. The
//! repair is never silent — [`AuthoredBody::notice`] and
//! [`AuthoredBody::print_notice`] report what changed and tell the caller to
//! look at the rendered draft before reporting the task done.

use envelope_email_transport::escapes::{EscapeAudit, normalize_literal_escapes};
use serde_json::{Value, json};

/// JSON key carrying the normalization report on every surface that has one.
pub(crate) const NOTICE_KEY: &str = "input_normalization";

/// An authored body (plain text and/or HTML) after the literal-escape check.
#[derive(Debug, Clone, Default)]
pub(crate) struct AuthoredBody {
    text: Option<String>,
    html: Option<String>,
    audits: Vec<(&'static str, EscapeAudit)>,
}

impl AuthoredBody {
    /// Check (and where unambiguous, repair) an authored body.
    pub(crate) fn new(text: Option<&str>, html: Option<&str>) -> Self {
        let mut audits = Vec::new();
        let mut checked = |field: &'static str, value: Option<&str>| -> Option<String> {
            let value = value?;
            let (normalized, audit) = normalize_literal_escapes(value);
            if let Some(audit) = audit {
                audits.push((field, audit));
            }
            Some(normalized.into_owned())
        };
        let text = checked("body", text);
        let html = checked("html", html);
        Self { text, html, audits }
    }

    /// Plain-text body to persist and send.
    pub(crate) fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// HTML body to persist and send.
    pub(crate) fn html(&self) -> Option<&str> {
        self.html.as_deref()
    }

    /// Whether anything is worth telling the caller about.
    pub(crate) fn has_notice(&self) -> bool {
        !self.audits.is_empty()
    }

    /// Whether a body was actually rewritten (as opposed to only flagged).
    fn repaired(&self) -> bool {
        self.audits.iter().any(|(_, a)| a.applied())
    }

    /// Machine-readable report for a `--json` / MCP result. `None` when the
    /// input was clean.
    pub(crate) fn notice(&self) -> Option<Value> {
        if !self.has_notice() {
            return None;
        }
        let fields: Vec<Value> = self
            .audits
            .iter()
            .map(|(field, audit)| {
                json!({
                    "field": field,
                    "action": if audit.applied() { "decoded" } else { "left_as_written" },
                    "newlines_converted": audit.newlines_converted,
                    "backslashes_unescaped": audit.backslashes_unescaped,
                    "newlines_left_as_written": audit.newlines_left_as_written,
                })
            })
            .collect();
        Some(json!({
            "applied": self.repaired(),
            "fields": fields,
            "explanation": self.explanation(),
            "verify": VERIFY,
        }))
    }

    /// Human lines for the non-JSON CLI output. Prints nothing when the input
    /// was clean. `review_url` is the draft's review page when there is one.
    pub(crate) fn print_notice(&self, review_url: Option<&str>) {
        if !self.has_notice() {
            return;
        }
        println!("  ⚠ {}", self.explanation());
        println!("    {VERIFY}");
        if let Some(url) = review_url {
            println!("    {url}");
        }
    }

    /// One sentence naming exactly what happened to which field.
    fn explanation(&self) -> String {
        let mut parts = Vec::new();
        for (field, audit) in &self.audits {
            if audit.applied() {
                let mut what = format!(
                    "{field}: {} literal \\n sequence{} arrived as text instead of line breaks and {} decoded",
                    audit.newlines_converted,
                    plural(audit.newlines_converted),
                    if audit.newlines_converted == 1 {
                        "was"
                    } else {
                        "were"
                    },
                );
                if audit.backslashes_unescaped > 0 {
                    what.push_str(&format!(
                        " ({} escaped backslash{} unescaped)",
                        audit.backslashes_unescaped,
                        if audit.backslashes_unescaped == 1 {
                            ""
                        } else {
                            "es"
                        },
                    ));
                }
                parts.push(what);
            } else {
                parts.push(format!(
                    "{field}: {} literal \\n sequence{} left exactly as written — the text already had real line breaks, so Envelope could not tell whether you meant them",
                    audit.newlines_left_as_written,
                    plural(audit.newlines_left_as_written),
                ));
            }
        }
        parts.join("; ")
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The instruction every normalization notice carries.
const VERIFY: &str = "Open the draft and read the final text before you report this task complete to your operator — escaped input usually means the rest of the body was assembled the same way.";

/// MCP tools whose `body`/`html` parameters carry agent-authored message
/// content. Anything listed here gets its normalization notice attached to the
/// tool result centrally, so a handler cannot forget to report a repair.
const AUTHORED_BODY_TOOLS: &[&str] = &[
    "send",
    "reply",
    "create_reply_draft",
    "create_forward_draft",
    "modify_draft",
];

/// Attach the notice for an MCP tool result, re-deriving it from the caller's
/// raw params (the check is pure, so this reproduces what the handler saw).
pub(crate) fn attach_tool_notice(tool: &str, params: &Value, result: &mut Value) {
    if !AUTHORED_BODY_TOOLS.contains(&tool) {
        return;
    }
    let authored = AuthoredBody::new(
        params.get("body").and_then(Value::as_str),
        params.get("html").and_then(Value::as_str),
    );
    attach_notice(result, &authored);
}

/// Add the notice (when there is one) to a JSON object result.
pub(crate) fn attach_notice(value: &mut Value, authored: &AuthoredBody) {
    if let (Some(obj), Some(notice)) = (value.as_object_mut(), authored.notice()) {
        obj.insert(NOTICE_KEY.to_string(), notice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_body_produces_no_notice() {
        let authored = AuthoredBody::new(Some("Hi,\n\nThanks"), None);
        assert_eq!(authored.text(), Some("Hi,\n\nThanks"));
        assert!(!authored.has_notice());
        assert!(authored.notice().is_none());
    }

    #[test]
    fn an_escaped_body_is_decoded_and_reported() {
        let authored = AuthoredBody::new(Some("Hi,\\n\\nThanks"), None);
        assert_eq!(authored.text(), Some("Hi,\n\nThanks"));
        let notice = authored.notice().expect("notice");
        assert_eq!(notice["applied"], json!(true));
        assert_eq!(notice["fields"][0]["field"], json!("body"));
        assert_eq!(notice["fields"][0]["action"], json!("decoded"));
        assert_eq!(notice["fields"][0]["newlines_converted"], json!(2));
        assert!(
            notice["verify"]
                .as_str()
                .expect("verify text")
                .contains("before you report this task complete"),
            "the notice must tell the agent to verify: {notice}"
        );
    }

    #[test]
    fn an_ambiguous_body_is_flagged_but_not_rewritten() {
        let raw = "Escape a newline with \\n in the shell.\nThat is all.";
        let authored = AuthoredBody::new(Some(raw), None);
        assert_eq!(authored.text(), Some(raw));
        let notice = authored.notice().expect("notice");
        assert_eq!(notice["applied"], json!(false));
        assert_eq!(notice["fields"][0]["action"], json!("left_as_written"));
        assert_eq!(notice["fields"][0]["newlines_left_as_written"], json!(1));
    }

    #[test]
    fn html_is_checked_too_and_named_separately() {
        let authored = AuthoredBody::new(Some("clean"), Some("<p>one</p>\\n<p>two</p>"));
        assert_eq!(authored.html(), Some("<p>one</p>\n<p>two</p>"));
        let notice = authored.notice().expect("notice");
        assert_eq!(notice["fields"].as_array().expect("fields").len(), 1);
        assert_eq!(notice["fields"][0]["field"], json!("html"));
    }

    #[test]
    fn attach_notice_adds_the_block_to_a_result_object() {
        let authored = AuthoredBody::new(Some("a\\nb"), None);
        let mut result = json!({"draft_id": "d1"});
        attach_notice(&mut result, &authored);
        assert_eq!(result[NOTICE_KEY]["applied"], json!(true));

        let mut clean_result = json!({"draft_id": "d2"});
        attach_notice(&mut clean_result, &AuthoredBody::new(Some("a\nb"), None));
        assert!(clean_result.get(NOTICE_KEY).is_none());
    }

    #[test]
    fn a_tool_result_gets_the_notice_only_for_authored_body_tools() {
        let params = json!({"body": "a\\nb"});
        let mut draft_result = json!({"draft_id": "d1"});
        attach_tool_notice("create_reply_draft", &params, &mut draft_result);
        assert_eq!(draft_result[NOTICE_KEY]["applied"], json!(true));

        let mut read_result = json!({"uid": 1});
        attach_tool_notice("read", &params, &mut read_result);
        assert!(read_result.get(NOTICE_KEY).is_none());
    }

    #[test]
    fn absent_bodies_are_absent_not_empty() {
        let authored = AuthoredBody::new(None, None);
        assert_eq!(authored.text(), None);
        assert_eq!(authored.html(), None);
        assert!(!authored.has_notice());
    }
}
