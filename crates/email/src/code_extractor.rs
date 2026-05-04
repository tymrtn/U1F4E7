// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Verification code extraction from email bodies.
//!
//! Scans plain-text and HTML bodies for common verification/OTP code
//! patterns. Returns the first code found (4-8 digits), preserving
//! leading zeros.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OtpPatternId {
    ExplicitLabel,
    OtpStyle,
    HtmlProminent,
    Fallback,
}

/// Extract a verification code from email text and optional HTML body.
///
/// Checks patterns in priority order:
/// 1. Explicit label (verification/confirmation/security code)
/// 2. OTP-style (one-time password, 2FA)
/// 3. HTML-prominent (bold, table cell)
/// 4. Fallback (isolated 4-8 digit number on its own line)
///
/// Returns the first code found as a String (preserving leading zeros).
pub fn extract_code(text: &str, html: Option<&str>) -> Option<String> {
    extract_code_with_pattern(text, html).map(|(code, _)| code)
}

/// Extract a verification code and the pattern category that matched.
pub fn extract_code_with_pattern(text: &str, html: Option<&str>) -> Option<(String, OtpPatternId)> {
    if let Some(caps) = explicit_regex().captures(text) {
        return Some((caps[1].to_string(), OtpPatternId::ExplicitLabel));
    }

    if let Some(caps) = otp_regex().captures(text) {
        return Some((caps[1].to_string(), OtpPatternId::OtpStyle));
    }

    if let Some(html_body) = html
        && let Some(caps) = html_regex().captures(html_body)
    {
        let code = caps.get(1).or_else(|| caps.get(2)).unwrap();
        return Some((code.as_str().to_string(), OtpPatternId::HtmlProminent));
    }

    if let Some(caps) = fallback_regex().captures(text) {
        return Some((caps[1].to_string(), OtpPatternId::Fallback));
    }

    None
}

/// Parse expiry hints such as "expires in 10 minutes" or "valid for 30 seconds".
pub fn parse_expiry_hint(text: &str) -> Option<u32> {
    let captures = expiry_regex().captures(text)?;
    let value: u32 = captures.get(1)?.as_str().parse().ok()?;
    let unit = captures.get(2)?.as_str().to_ascii_lowercase();
    let seconds = match unit.as_str() {
        "second" | "seconds" | "sec" | "secs" => value,
        "minute" | "minutes" | "min" | "mins" => value.saturating_mul(60),
        "hour" | "hours" | "hr" | "hrs" => value.saturating_mul(60 * 60),
        _ => return None,
    };
    Some(seconds)
}

/// Redact OTP-shaped digit sequences from text before persistence or event delivery.
pub fn redact_codes(text: &str) -> String {
    redaction_regex().replace_all(text, "***").into_owned()
}

fn redaction_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"\b\d(?:[ -]?\d){3,7}\b|\b\d{3}[- ]?\d{3}\b|\b\d{4,8}\b").unwrap()
    })
}

fn explicit_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?i)(?:verification|confirmation|security|auth(?:entication)?|login)\s*(?:code|number|pin)\s*(?:is|:)?\s*(\d{4,8})",
        )
        .unwrap()
    })
}

fn otp_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?i)(?:one.time|OTP|2FA|two.factor)\s*(?:code|password|passcode|pin)\s*(?:is|:)?\s*(\d{4,8})",
        )
        .unwrap()
    })
}

fn html_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)<(?:strong|b)>(\d{4,8})</(?:strong|b)>|<td[^>]*>(\d{4,8})</td>").unwrap()
    })
}

fn fallback_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?m)^\s*(\d{4,8})\s*$").unwrap())
}

fn expiry_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?i)(?:expires?|valid(?:ity)?)(?:\s+(?:in|for))?\s+(\d{1,3})\s+(seconds?|secs?|minutes?|mins?|hours?|hrs?)\b",
        )
        .unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_verification_code() {
        let text = "Your verification code is 847291";
        assert_eq!(extract_code(text, None), Some("847291".to_string()));
    }

    #[test]
    fn explicit_confirmation_code_with_colon() {
        let text = "Confirmation code: 1234";
        assert_eq!(extract_code(text, None), Some("1234".to_string()));
    }

    #[test]
    fn explicit_security_code() {
        let text = "Your security code is 00482913";
        assert_eq!(extract_code(text, None), Some("00482913".to_string()));
    }

    #[test]
    fn explicit_auth_pin() {
        let text = "Enter your authentication pin: 9012";
        assert_eq!(extract_code(text, None), Some("9012".to_string()));
    }

    #[test]
    fn explicit_login_code() {
        let text = "Your login code is 556677";
        assert_eq!(extract_code(text, None), Some("556677".to_string()));
    }

    #[test]
    fn otp_code() {
        let text = "Your OTP code is 482910";
        assert_eq!(extract_code(text, None), Some("482910".to_string()));
    }

    #[test]
    fn two_factor_passcode() {
        let text = "Your two-factor passcode: 7731";
        assert_eq!(extract_code(text, None), Some("7731".to_string()));
    }

    #[test]
    fn one_time_password() {
        let text = "Use your one-time password 123456 to log in.";
        assert_eq!(extract_code(text, None), Some("123456".to_string()));
    }

    #[test]
    fn html_strong_code() {
        let text = "Check your email for the code.";
        let html = Some("<p>Your code is <strong>904821</strong></p>");
        assert_eq!(extract_code(text, html), Some("904821".to_string()));
    }

    #[test]
    fn html_bold_code() {
        let text = "We sent you a code.";
        let html = Some("<p>Code: <b>5544</b></p>");
        assert_eq!(extract_code(text, html), Some("5544".to_string()));
    }

    #[test]
    fn html_td_code() {
        let text = "Verify your account.";
        let html = Some(r#"<table><tr><td class="code">77889900</td></tr></table>"#);
        assert_eq!(extract_code(text, html), Some("77889900".to_string()));
    }

    #[test]
    fn fallback_isolated_line() {
        let text = "Hello,\n\nPlease use the following:\n\n  829104\n\nThanks!";
        assert_eq!(extract_code(text, None), Some("829104".to_string()));
    }

    #[test]
    fn fallback_preserves_leading_zeros() {
        let text = "Your code:\n0042\n";
        assert_eq!(extract_code(text, None), Some("0042".to_string()));
    }

    #[test]
    fn no_code_in_text() {
        let text = "Hello, this is a normal email with no codes.";
        assert_eq!(extract_code(text, None), None);
    }

    #[test]
    fn short_number_not_matched() {
        let text = "Your code is 123";
        assert_eq!(extract_code(text, None), None);
    }

    #[test]
    fn long_number_not_matched() {
        let text = "Your code is 123456789";
        assert_eq!(extract_code(text, None), None);
    }

    #[test]
    fn explicit_label_takes_priority_over_fallback() {
        let text = "Your verification code is 111111\n\n222222\n";
        assert_eq!(extract_code(text, None), Some("111111".to_string()));
    }

    #[test]
    fn otp_takes_priority_over_html() {
        let text = "Your OTP code is 333333";
        let html = Some("<strong>444444</strong>");
        assert_eq!(extract_code(text, html), Some("333333".to_string()));
    }

    #[test]
    fn extract_code_reports_pattern() {
        let (code, pattern) =
            extract_code_with_pattern("Your verification code is 111111\n222222", None).unwrap();
        assert_eq!(code, "111111");
        assert_eq!(pattern, OtpPatternId::ExplicitLabel);
    }

    #[test]
    fn parse_expiry_hint_in_minutes() {
        assert_eq!(
            parse_expiry_hint("This code expires in 10 minutes."),
            Some(600)
        );
    }

    #[test]
    fn parse_expiry_hint_in_seconds() {
        assert_eq!(parse_expiry_hint("Valid for 30 seconds"), Some(30));
    }

    #[test]
    fn redact_codes_handles_contiguous_and_separated_codes() {
        assert_eq!(
            redact_codes("Use 123456 or 123-456 or 123 456 or 1 2 3 4 5 6"),
            "Use *** or *** or *** or ***"
        );
    }
}
