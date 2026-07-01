// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::Result;

pub const RE_SUBJECT_WITHOUT_THREAD_CODE: &str = "re_subject_without_thread_context";
const RE_SUBJECT_WITHOUT_THREAD_REASON: &str =
    "subject begins with a reply prefix but no reply/thread context was supplied";

const REPLY_SUBJECT_PREFIXES: &[&str] = &[
    "re:",   // English reply
    "aw:",   // German Antwort
    "sv:",   // Swedish/Norwegian svar
    "odp:",  // Polish odpowiedz
    "antw:", // German Antwort variant
];

pub fn subject_starts_with_reply_prefix(subject: &str) -> bool {
    let trimmed = subject.trim_start().to_lowercase();
    REPLY_SUBJECT_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

pub fn check_new_re_subject_guard(
    subject: Option<&str>,
    has_thread_context: bool,
    confirmed_new_re_subject: bool,
    json: bool,
) -> Result<()> {
    if has_thread_context || confirmed_new_re_subject {
        return Ok(());
    }

    let Some(subject) = subject else {
        return Ok(());
    };

    if !subject_starts_with_reply_prefix(subject) {
        return Ok(());
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "denied",
                "error": {
                    "code": RE_SUBJECT_WITHOUT_THREAD_CODE,
                    "reason": RE_SUBJECT_WITHOUT_THREAD_REASON,
                    "suggestion": "Use Envelope MCP/agent reply or CLI draft reply with source UID; use --in-reply-to for explicit low-level draft creation, or re-run with --confirm-new-re-subject if this is intentionally a new message."
                }
            })
        );
    }

    anyhow::bail!(
        "{RE_SUBJECT_WITHOUT_THREAD_CODE}: {RE_SUBJECT_WITHOUT_THREAD_REASON}. Use Envelope MCP/agent reply or CLI draft reply with source UID; use --in-reply-to for explicit low-level draft creation, or re-run with --confirm-new-re-subject if this is intentionally a new message."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_re_subject_case_and_whitespace() {
        assert!(subject_starts_with_reply_prefix("Re: hello"));
        assert!(subject_starts_with_reply_prefix("  re: hello"));
        assert!(subject_starts_with_reply_prefix("AW: hallo"));
        assert!(!subject_starts_with_reply_prefix("Report: hello"));
        assert!(!subject_starts_with_reply_prefix("Hello"));
    }

    #[test]
    fn blocks_re_subject_without_thread_context_or_confirmation() {
        let err = check_new_re_subject_guard(Some("Re: hello"), false, false, false)
            .expect_err("Re: subject without context should be blocked");
        assert!(err.to_string().contains(RE_SUBJECT_WITHOUT_THREAD_CODE));
    }

    #[test]
    fn allows_re_subject_with_thread_context() {
        check_new_re_subject_guard(Some("Re: hello"), true, false, false)
            .expect("thread context should allow Re: subject");
    }

    #[test]
    fn allows_re_subject_with_explicit_confirmation() {
        check_new_re_subject_guard(Some("  re: hello"), false, true, false)
            .expect("explicit confirmation should allow Re: subject");
    }

    #[test]
    fn allows_non_re_subject_without_confirmation() {
        check_new_re_subject_guard(Some("Status update"), false, false, false)
            .expect("non-Re subject should be unaffected");
    }
}
