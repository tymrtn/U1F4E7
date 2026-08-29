// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Repair for authored bodies that arrive carrying literal escape sequences.
//!
//! An agent composing through a shell writes `--body "Hi,\n\nThanks"`, and the
//! shell hands Envelope the two characters `\` and `n` — not a line break. The
//! draft then stores, appends, and eventually sends a wall of text with visible
//! `\n` markers in it. The same accident reaches the JSON surfaces when a caller
//! double-encodes a string (`"\\n"`).
//!
//! [`normalize_literal_escapes`] repairs that ONE unambiguous case and reports
//! what it did, so the surface can tell the caller to look at the result:
//!
//! - The input carries literal newline escapes and **no real line break at all**
//!   → the escapes are decoded and the audit says how many.
//! - The input carries literal newline escapes **alongside real line breaks**
//!   → nothing is rewritten (the `\n` may be deliberate prose about code), and
//!   the audit reports the leftovers so the surface can warn.
//! - No literal newline escape → no audit, no change.
//!
//! Only newline escapes trigger a repair. `\t` is left alone: a tab is rare in
//! an email body and common in a Windows path (`C:\temp`), so decoding it costs
//! more than it saves. `\\` IS decoded once a repair is triggered, which keeps
//! the model consistent and gives callers an escape hatch — write `\\n` to keep
//! a literal backslash-n in the text.
//!
//! Pure and deterministic: no IO, no clock.

use std::borrow::Cow;

/// What [`normalize_literal_escapes`] found in one authored string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EscapeAudit {
    /// Literal newline escapes (`\n`, `\r`, `\r\n`) decoded into line breaks.
    pub newlines_converted: usize,
    /// Escaped backslashes (`\\`) decoded to a single backslash.
    pub backslashes_unescaped: usize,
    /// Literal newline escapes left exactly as written because the text already
    /// contained real line breaks, so the intent is ambiguous.
    pub newlines_left_as_written: usize,
}

impl EscapeAudit {
    /// Whether the text was actually rewritten.
    pub fn applied(&self) -> bool {
        self.newlines_converted > 0 || self.backslashes_unescaped > 0
    }
}

/// Decode literal escape sequences in an authored body when — and only when —
/// they are unambiguously an encoding accident. See the module docs.
///
/// Returns the text to use plus an audit when there is something to report.
/// `None` means the input was clean and nothing happened.
pub fn normalize_literal_escapes(input: &str) -> (Cow<'_, str>, Option<EscapeAudit>) {
    let (decoded, audit) = decode(input);
    if audit.newlines_converted == 0 {
        // No literal newline escape: nothing to repair and nothing to warn about.
        // A stray `\\` on its own is not evidence of an encoding accident.
        return (Cow::Borrowed(input), None);
    }
    if input.contains('\n') {
        // Real line breaks are already present, so the `\n` sequences may be
        // deliberate. Leave the text alone and let the surface say so.
        return (
            Cow::Borrowed(input),
            Some(EscapeAudit {
                newlines_left_as_written: audit.newlines_converted,
                ..EscapeAudit::default()
            }),
        );
    }
    (Cow::Owned(decoded), Some(audit))
}

/// Decode `\n`, `\r`, `\r\n`, and `\\`, counting each. Every other backslash
/// pair is copied through untouched.
fn decode(input: &str) -> (String, EscapeAudit) {
    let mut out = String::with_capacity(input.len());
    let mut audit = EscapeAudit::default();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => {
                    out.push('\n');
                    audit.newlines_converted += 1;
                    i += 2;
                    continue;
                }
                b'r' => {
                    i += 2;
                    // A literal CRLF pair is one line break, not two.
                    if i + 1 < bytes.len() && bytes[i] == b'\\' && bytes[i + 1] == b'n' {
                        i += 2;
                    }
                    out.push('\n');
                    audit.newlines_converted += 1;
                    continue;
                }
                b'\\' => {
                    out.push('\\');
                    audit.backslashes_unescaped += 1;
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        // `\` and the escape letters are ASCII, so `i` is always on a character
        // boundary here; copy one whole character.
        let ch = input[i..]
            .chars()
            .next()
            .expect("index is on a character boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, audit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_a_body_whose_only_line_breaks_are_literal() {
        let raw = "Hi Alexander,\\n\\nThank you for completing the questionnaire.\\n\\nTyler";
        let (text, audit) = normalize_literal_escapes(raw);
        assert_eq!(
            text,
            "Hi Alexander,\n\nThank you for completing the questionnaire.\n\nTyler"
        );
        let audit = audit.expect("an audit is reported");
        assert!(audit.applied());
        assert_eq!(audit.newlines_converted, 4);
        assert_eq!(audit.newlines_left_as_written, 0);
    }

    #[test]
    fn leaves_a_clean_body_untouched_and_silent() {
        let raw = "Hi Alexander,\n\nThanks.\n\nTyler";
        let (text, audit) = normalize_literal_escapes(raw);
        assert_eq!(text, raw);
        assert!(audit.is_none());
    }

    #[test]
    fn a_mixed_body_is_reported_but_never_rewritten() {
        let raw = "Use \\n for a line break.\nThat is the whole trick.";
        let (text, audit) = normalize_literal_escapes(raw);
        assert_eq!(text, raw, "ambiguous text must survive verbatim");
        let audit = audit.expect("an audit is reported");
        assert!(!audit.applied());
        assert_eq!(audit.newlines_left_as_written, 1);
        assert_eq!(audit.newlines_converted, 0);
    }

    #[test]
    fn an_escaped_backslash_keeps_a_literal_newline_marker() {
        let (text, audit) = normalize_literal_escapes("first\\nliteral: \\\\n stays");
        assert_eq!(text, "first\nliteral: \\n stays");
        let audit = audit.expect("an audit is reported");
        assert_eq!(audit.newlines_converted, 1);
        assert_eq!(audit.backslashes_unescaped, 1);
    }

    #[test]
    fn a_literal_crlf_pair_becomes_one_line_break() {
        let (text, audit) = normalize_literal_escapes("one\\r\\ntwo\\rthree");
        assert_eq!(text, "one\ntwo\nthree");
        assert_eq!(audit.expect("audit").newlines_converted, 2);
    }

    #[test]
    fn a_windows_path_is_not_mangled_when_nothing_triggers_a_repair() {
        let raw = "Copy it to C:\\temp and tell me when it lands.";
        let (text, audit) = normalize_literal_escapes(raw);
        assert_eq!(text, raw);
        assert!(audit.is_none(), "a lone \\t is not an encoding accident");
    }

    #[test]
    fn unknown_escapes_and_tabs_survive_a_repair() {
        let (text, _) = normalize_literal_escapes("path C:\\temp\\nnext line");
        assert_eq!(text, "path C:\\temp\nnext line");
    }

    #[test]
    fn multibyte_text_survives() {
        let (text, audit) = normalize_literal_escapes("Hasta pronto — José\\nSaludos ✉️");
        assert_eq!(text, "Hasta pronto — José\nSaludos ✉️");
        assert_eq!(audit.expect("audit").newlines_converted, 1);
    }

    #[test]
    fn a_trailing_lone_backslash_is_kept() {
        let (text, _) = normalize_literal_escapes("ends with a backslash\\nand then \\");
        assert_eq!(text, "ends with a backslash\nand then \\");
    }

    #[test]
    fn empty_input_is_silent() {
        let (text, audit) = normalize_literal_escapes("");
        assert_eq!(text, "");
        assert!(audit.is_none());
    }
}
