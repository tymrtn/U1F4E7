// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Pure compose primitives for contextual reply/forward drafts.
//!
//! These helpers build the *preserved context block* (the quoted parent for a
//! reply, or the forwarded message for a forward), assemble a full draft body
//! from an agent-authored part + optional signature + that context block, and
//! produce an abridged preview so an agent can see what it is quoting without
//! carrying the full prior thread in its prompt.
//!
//! Everything here is pure and deterministic — no IMAP, no SMTP, no clock — so
//! it can be unit-tested exhaustively. Transport/storage wiring lives in the
//! CLI and MCP layers.
//!
//! The split is deliberate:
//! - [`build_reply_context`] / [`build_forward_context`] produce a
//!   [`ContextBlock`] *once* at draft creation from the parent message.
//! - The [`ContextBlock`] is persisted in the local draft metadata.
//! - [`assemble_body`] recombines a (possibly edited) agent body with the
//!   signature and the preserved [`ContextBlock`]. On modify, the agent only
//!   replaces its authored part; the quote/forward block is never lost.

use envelope_email_store::models::Message;

use crate::threading::strip_reply_prefixes;

/// Default maximum number of words shown in an abridged quote/forward preview.
pub const DEFAULT_PREVIEW_WORD_LIMIT: usize = 300;

/// RFC 3676 signature delimiter (note the trailing space — it is significant).
const SIGNATURE_DELIMITER: &str = "-- ";

/// What kind of draft a context block belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftKind {
    Reply,
    Forward,
    New,
}

impl DraftKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DraftKind::Reply => "reply",
            DraftKind::Forward => "forward",
            DraftKind::New => "new",
        }
    }
}

/// The preserved context block carried by a contextual draft.
///
/// For a reply this is the quoted parent message; for a forward it is the
/// forwarded message block. Stored verbatim in draft metadata so a modify can
/// reconstruct the full body after the agent edits only its authored portion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextBlock {
    /// Plain-text rendering of the block (empty when there is no context).
    pub text: String,
    /// HTML rendering of the block, present only when the source had HTML.
    pub html: Option<String>,
    /// Stable format tag: `plain_prefix`, `gmail_quote`, or `forward`.
    pub format: String,
    /// Whether a context block is actually present.
    pub included: bool,
}

/// The assembled bodies plus signature state for a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledBody {
    pub text: String,
    pub html: Option<String>,
    pub signature_applied: bool,
}

/// Build the abridged preview for a source body.
///
/// Returns `(preview, truncated)` where `preview` is at most `max_words`
/// whitespace-delimited words. `truncated` is true when words were dropped.
/// A `max_words` of 0 is treated as 1 (always keep at least one word if any).
pub fn abridge_words(text: &str, max_words: usize) -> (String, bool) {
    let max = max_words.max(1);
    let mut words = text.split_whitespace();
    let mut kept: Vec<&str> = Vec::with_capacity(max);
    for _ in 0..max {
        match words.next() {
            Some(w) => kept.push(w),
            None => break,
        }
    }
    let truncated = words.next().is_some();
    let mut preview = kept.join(" ");
    if truncated {
        preview.push('…');
    }
    (preview, truncated)
}

/// Best-effort plain text from a message: prefer the text body, otherwise
/// derive a stripped version of the HTML body.
pub fn message_preview_source(parent: &Message) -> String {
    if let Some(text) = parent.text_body.as_deref() {
        if !text.trim().is_empty() {
            return text.to_string();
        }
    }
    if let Some(html) = parent.html_body.as_deref() {
        return strip_html(html);
    }
    String::new()
}

/// Build the quoted-reply context block from a parent message.
///
/// Plain text uses the `>`-prefix convention under an attribution line. HTML is
/// only produced when the parent actually had an HTML body (mirroring Mail.app
/// / Gmail behavior); otherwise the block is text-only.
pub fn build_reply_context(parent: &Message) -> ContextBlock {
    let attribution = attribution_line(parent);
    let source_text = message_preview_source(parent);

    let mut text = String::new();
    text.push_str(&attribution);
    text.push('\n');
    for line in source_text.lines() {
        text.push_str("> ");
        text.push_str(line);
        text.push('\n');
    }
    // Trim a single trailing newline for stable equality/round-trips.
    let text = text.trim_end_matches('\n').to_string();

    let html = parent.html_body.as_deref().map(|body| {
        format!(
            "<div class=\"gmail_quote\">\n  <div>{}</div>\n  <blockquote class=\"gmail_quote\" \
             style=\"margin:0 0 0 .8ex;border-left:1px solid #ccc;padding-left:1ex\">\n{}\n  \
             </blockquote>\n</div>",
            html_escape(&attribution),
            body
        )
    });

    let format = if html.is_some() {
        "gmail_quote"
    } else {
        "plain_prefix"
    };

    ContextBlock {
        text,
        html,
        format: format.to_string(),
        included: true,
    }
}

/// Build the forwarded-message context block from a parent message.
pub fn build_forward_context(parent: &Message) -> ContextBlock {
    let source_text = message_preview_source(parent);

    let mut header_lines = vec![
        format!("From: {}", parent.from_addr),
        format!("Date: {}", parent.date.as_deref().unwrap_or("")),
        format!("Subject: {}", parent.subject),
        format!("To: {}", parent.to_addr),
    ];
    if let Some(cc) = parent.cc_addr.as_deref() {
        if !cc.trim().is_empty() {
            header_lines.push(format!("Cc: {cc}"));
        }
    }

    let mut text = String::new();
    text.push_str("---------- Forwarded message ---------\n");
    for line in &header_lines {
        text.push_str(line);
        text.push('\n');
    }
    text.push('\n');
    text.push_str(&source_text);
    let text = text.trim_end_matches('\n').to_string();

    let html = parent.html_body.as_deref().map(|body| {
        let header_html = header_lines
            .iter()
            .map(|l| format!("<div>{}</div>", html_escape(l)))
            .collect::<Vec<_>>()
            .join("\n  ");
        format!(
            "<div class=\"envelope-forward\">\n  <div>---------- Forwarded message \
             ---------</div>\n  {}\n  <br>\n  <div class=\"envelope-forward-body\">{}</div>\n</div>",
            header_html, body
        )
    });

    ContextBlock {
        text,
        html,
        format: "forward".to_string(),
        included: true,
    }
}

/// Assemble a full draft body from the agent-authored part, an optional
/// signature, and the preserved context block.
///
/// `add_signature` only takes effect when a non-empty `signature_text` (or
/// `signature_html`) is supplied. The returned `signature_applied` reflects what
/// actually happened.
///
/// HTML output is produced when the agent supplied HTML *or* the context block
/// carries HTML — so an HTML quote is never silently downgraded to text-only.
pub fn assemble_body(
    agent_text: &str,
    agent_html: Option<&str>,
    signature_text: Option<&str>,
    signature_html: Option<&str>,
    add_signature: bool,
    context: &ContextBlock,
) -> AssembledBody {
    let sig_text = signature_text.filter(|s| !s.trim().is_empty());
    let sig_html = signature_html.filter(|s| !s.trim().is_empty());
    let signature_applied = add_signature && (sig_text.is_some() || sig_html.is_some());

    // ── text ──
    let mut blocks: Vec<String> = Vec::new();
    blocks.push(agent_text.trim_end().to_string());
    if add_signature {
        if let Some(sig) = sig_text {
            blocks.push(format!("{SIGNATURE_DELIMITER}\n{}", sig.trim_end()));
        }
    }
    if context.included && !context.text.is_empty() {
        blocks.push(context.text.clone());
    }
    let text = blocks.join("\n\n");

    // ── html ──
    let want_html = agent_html.is_some() || context.html.is_some();
    let html = if want_html {
        let agent_html_part = match agent_html {
            Some(h) => h.to_string(),
            None => format!("<div>{}</div>", text_to_html(agent_text)),
        };
        let mut out = format!("<div class=\"envelope-agent-body\">{agent_html_part}</div>");
        if add_signature {
            if let Some(sig) = sig_html.or(sig_text) {
                let sig_rendered = if signature_html.is_some() {
                    sig.to_string()
                } else {
                    text_to_html(sig)
                };
                out.push_str(&format!(
                    "\n<div class=\"envelope-signature\">{sig_rendered}</div>"
                ));
            }
        }
        if let Some(ctx_html) = &context.html {
            out.push_str("\n<br>\n");
            out.push_str(ctx_html);
        } else if context.included && !context.text.is_empty() {
            // Context was text-only but we are emitting HTML; preserve it as a
            // pre-formatted block so the quote is not lost in the HTML part.
            out.push_str(&format!(
                "\n<br>\n<pre class=\"envelope-quote-text\">{}</pre>",
                html_escape(&context.text)
            ));
        }
        Some(out)
    } else {
        None
    };

    AssembledBody {
        text,
        html,
        signature_applied,
    }
}

/// Idempotent `Fwd: ` subject prefix. Strips any existing reply/forward
/// prefixes first so `Fwd: Re: Fwd: x` collapses to `Fwd: x`.
pub fn prefix_forward_subject(subject: &str) -> String {
    let stripped = strip_reply_prefixes(subject);
    if stripped.is_empty() {
        "Fwd:".to_string()
    } else {
        format!("Fwd: {stripped}")
    }
}

// ── internal helpers ────────────────────────────────────────────────

/// Build the `On <date> <from> wrote:` attribution line.
fn attribution_line(parent: &Message) -> String {
    let from = parent.from_addr.trim();
    match parent
        .date
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(date) => format!("On {date}, {from} wrote:"),
        None => format!("{from} wrote:"),
    }
}

/// Minimal HTML escaping for text inserted into HTML output.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Convert plain text to minimal HTML: escape, then newlines to `<br>`.
fn text_to_html(s: &str) -> String {
    html_escape(s).replace('\n', "<br>\n")
}

/// Extremely small HTML-to-text reducer used only for preview fallback when a
/// message has no text body. Drops tags and collapses whitespace; it is not a
/// general-purpose renderer.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    // Decode a couple of the most common entities for readability.
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelope_email_store::models::{AttachmentMeta, Message};

    fn make_parent(
        subject: &str,
        from: &str,
        to: &str,
        cc: Option<&str>,
        text: Option<&str>,
        html: Option<&str>,
    ) -> Message {
        Message {
            uid: 1,
            message_id: Some("<parent@example.com>".to_string()),
            from_addr: from.to_string(),
            to_addr: to.to_string(),
            cc_addr: cc.map(str::to_string),
            subject: subject.to_string(),
            date: Some("Tue, 4 Jun 2026 10:31:00 +0000".to_string()),
            text_body: text.map(str::to_string),
            html_body: html.map(str::to_string),
            in_reply_to: None,
            references: None,
            flags: vec![],
            attachments: Vec::<AttachmentMeta>::new(),
        }
    }

    // ── abridge_words ────────────────────────────────────────────────

    #[test]
    fn abridge_under_limit_is_not_truncated() {
        let (preview, truncated) = abridge_words("one two three", 300);
        assert_eq!(preview, "one two three");
        assert!(!truncated);
    }

    #[test]
    fn abridge_at_limit_is_not_truncated() {
        let (preview, truncated) = abridge_words("one two three", 3);
        assert_eq!(preview, "one two three");
        assert!(!truncated);
    }

    #[test]
    fn abridge_over_limit_marks_truncated_and_appends_ellipsis() {
        let (preview, truncated) = abridge_words("a b c d e", 3);
        assert_eq!(preview, "a b c…");
        assert!(truncated);
    }

    #[test]
    fn abridge_collapses_whitespace_to_single_spaces() {
        let (preview, _) = abridge_words("  a   b\n\nc\t d  ", 10);
        assert_eq!(preview, "a b c d");
    }

    #[test]
    fn abridge_empty_text() {
        let (preview, truncated) = abridge_words("", 300);
        assert_eq!(preview, "");
        assert!(!truncated);
    }

    #[test]
    fn abridge_300_word_limit_default() {
        let words = (0..400)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let (preview, truncated) = abridge_words(&words, DEFAULT_PREVIEW_WORD_LIMIT);
        assert!(truncated);
        // 300 words kept (plus the ellipsis appended to the last).
        assert_eq!(preview.split_whitespace().count(), 300);
        assert!(preview.ends_with('…'));
    }

    // ── build_reply_context ──────────────────────────────────────────

    #[test]
    fn reply_context_text_prefixes_each_line() {
        let parent = make_parent(
            "Hi",
            "Alice <alice@example.com>",
            "bob@example.com",
            None,
            Some("line one\nline two"),
            None,
        );
        let ctx = build_reply_context(&parent);
        assert!(ctx.included);
        assert_eq!(ctx.format, "plain_prefix");
        assert!(ctx.html.is_none());
        assert_eq!(
            ctx.text,
            "On Tue, 4 Jun 2026 10:31:00 +0000, Alice <alice@example.com> wrote:\n> line one\n> line two"
        );
    }

    #[test]
    fn reply_context_html_present_when_parent_has_html() {
        let parent = make_parent(
            "Hi",
            "alice@example.com",
            "bob@example.com",
            None,
            Some("plain"),
            Some("<p>rich</p>"),
        );
        let ctx = build_reply_context(&parent);
        assert_eq!(ctx.format, "gmail_quote");
        let html = ctx.html.expect("html quote");
        assert!(html.contains("gmail_quote"));
        assert!(html.contains("<p>rich</p>"));
        assert!(html.contains("wrote:"));
    }

    #[test]
    fn reply_context_attribution_without_date() {
        let mut parent = make_parent(
            "Hi",
            "alice@example.com",
            "bob@example.com",
            None,
            Some("x"),
            None,
        );
        parent.date = None;
        let ctx = build_reply_context(&parent);
        assert!(ctx.text.starts_with("alice@example.com wrote:"));
    }

    // ── build_forward_context ────────────────────────────────────────

    #[test]
    fn forward_context_includes_headers_and_body() {
        let parent = make_parent(
            "Quarterly numbers",
            "Alice <alice@example.com>",
            "bob@example.com",
            Some("carol@example.com"),
            Some("the body"),
            None,
        );
        let ctx = build_forward_context(&parent);
        assert_eq!(ctx.format, "forward");
        assert!(ctx.text.contains("---------- Forwarded message ---------"));
        assert!(ctx.text.contains("From: Alice <alice@example.com>"));
        assert!(ctx.text.contains("Subject: Quarterly numbers"));
        assert!(ctx.text.contains("To: bob@example.com"));
        assert!(ctx.text.contains("Cc: carol@example.com"));
        assert!(ctx.text.contains("the body"));
    }

    #[test]
    fn forward_context_omits_cc_when_absent() {
        let parent = make_parent("S", "a@x", "b@x", None, Some("body"), None);
        let ctx = build_forward_context(&parent);
        assert!(!ctx.text.contains("Cc:"));
    }

    // ── assemble_body ────────────────────────────────────────────────

    #[test]
    fn assemble_reply_without_signature_keeps_quote() {
        let parent = make_parent(
            "Hi",
            "alice@example.com",
            "bob@example.com",
            None,
            Some("orig"),
            None,
        );
        let ctx = build_reply_context(&parent);
        let body = assemble_body("My reply.", None, None, None, false, &ctx);
        assert!(!body.signature_applied);
        assert!(body.text.starts_with("My reply."));
        assert!(body.text.contains("> orig"));
        assert!(body.html.is_none());
    }

    #[test]
    fn assemble_applies_signature_with_delimiter() {
        let ctx = ContextBlock::default();
        let body = assemble_body("Hello.", None, Some("Tyler\nEnvelope"), None, true, &ctx);
        assert!(body.signature_applied);
        assert!(body.text.contains("\n-- \nTyler\nEnvelope"));
    }

    #[test]
    fn assemble_signature_requested_but_empty_is_not_applied() {
        let ctx = ContextBlock::default();
        let body = assemble_body("Hello.", None, Some("   "), None, true, &ctx);
        assert!(!body.signature_applied);
        assert!(!body.text.contains("-- "));
    }

    #[test]
    fn assemble_signature_not_requested_is_not_applied() {
        let ctx = ContextBlock::default();
        let body = assemble_body("Hello.", None, Some("Tyler"), None, false, &ctx);
        assert!(!body.signature_applied);
        assert!(!body.text.contains("Tyler"));
    }

    #[test]
    fn assemble_produces_html_when_context_has_html() {
        let parent = make_parent("Hi", "a@x", "b@x", None, Some("p"), Some("<p>rich</p>"));
        let ctx = build_reply_context(&parent);
        let body = assemble_body("Reply body", None, None, None, false, &ctx);
        let html = body.html.expect("html should be produced");
        assert!(html.contains("envelope-agent-body"));
        assert!(html.contains("Reply body"));
        assert!(html.contains("<p>rich</p>"));
    }

    #[test]
    fn assemble_text_only_context_preserved_in_html_when_agent_html() {
        let parent = make_parent("Hi", "a@x", "b@x", None, Some("orig text"), None);
        let ctx = build_reply_context(&parent);
        let body = assemble_body(
            "Reply body",
            Some("<b>Reply body</b>"),
            None,
            None,
            false,
            &ctx,
        );
        let html = body.html.expect("html should be produced");
        assert!(html.contains("<b>Reply body</b>"));
        assert!(html.contains("envelope-quote-text"));
        assert!(html.contains("&gt; orig text"));
    }

    #[test]
    fn assemble_round_trips_after_edit_preserving_quote() {
        let parent = make_parent(
            "Hi",
            "alice@example.com",
            "bob@example.com",
            None,
            Some("orig"),
            None,
        );
        let ctx = build_reply_context(&parent);
        let first = assemble_body("Draft one.", None, None, None, false, &ctx);
        let edited = assemble_body(
            "Draft two — totally rewritten.",
            None,
            None,
            None,
            false,
            &ctx,
        );
        assert!(first.text.contains("> orig"));
        assert!(edited.text.contains("> orig"));
        assert!(edited.text.starts_with("Draft two"));
        assert!(!edited.text.contains("Draft one"));
    }

    // ── prefix_forward_subject ───────────────────────────────────────

    #[test]
    fn forward_subject_is_idempotent() {
        assert_eq!(prefix_forward_subject("Report"), "Fwd: Report");
        assert_eq!(prefix_forward_subject("Fwd: Report"), "Fwd: Report");
        assert_eq!(prefix_forward_subject("Fwd: Fwd: Report"), "Fwd: Report");
        assert_eq!(prefix_forward_subject("Re: Report"), "Fwd: Report");
    }
}
