// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Server-side HTML sanitizer (ammonia), the same policy as the reader's
//! `crates/dashboard/web/src/lib/components/rich-html.ts`:
//!
//! - active and embedding elements are removed with their content;
//! - `on*` handlers and navigation/loading attributes are dropped (ammonia's
//!   attribute allowlist);
//! - links keep only `http`, `https`, `mailto`, `tel` and `#fragment`;
//! - images keep only `cid:` and raster `data:image/*`; remote images never
//!   load (their `src` is dropped);
//! - inline `style` survives unless it could load or execute anything.
//!
//! Both implementations run `sanitize-fixtures.json` (next to rich-html.ts).
//! CLI and MCP `read` serve `dangerous` mail through this with
//! `sanitized: true`.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

/// Removed together with everything inside them (rich-html.ts
/// `DANGEROUS_ELEMENTS`, plus `head`/`title` so only body text survives).
const REMOVED_WITH_CONTENT: &[&str] = &[
    "script", "style", "link", "form", "input", "button", "textarea", "select", "option", "iframe",
    "frame", "frameset", "object", "embed", "applet", "meta", "base", "template", "area", "audio",
    "video", "source", "track", "canvas", "svg", "math", "head", "title", "noscript",
];

const EXTRA_TAGS: &[&str] = &["font", "tfoot"];
const EXTRA_GENERIC_ATTRIBUTES: &[&str] = &["style", "align", "dir", "width", "height"];
const EXTRA_TABLE_ATTRIBUTES: &[&str] = &[
    "width",
    "height",
    "cellpadding",
    "cellspacing",
    "border",
    "bgcolor",
    "valign",
];

static DANGEROUS_CSS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"\\",
        r"(?i)@import\b",
        r"(?i)url\s*\(",
        r"(?i)(?:-webkit-)?image-set\s*\(",
        r"(?i)(?:https?|data|blob|file)\s*:",
        r"(?i)expression\s*\(",
        r"(?i)(?:behavior|-moz-binding)\s*:",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("css policy regex"))
    .collect()
});
static SAFE_INLINE_IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:cid:|data:image/(?:png|gif|jpe?g|webp|avif)[;,])").expect("image regex")
});

/// Mirrors `hasDangerousCss` in rich-html.ts.
pub fn has_dangerous_css(value: &str) -> bool {
    DANGEROUS_CSS.iter().any(|re| re.is_match(value))
}

/// Browsers drop ASCII controls and spaces while reading a scheme; do the
/// same before judging one (`java\nscript:`).
fn compact_url(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|c| !(c.is_control() || *c == ' '))
        .collect()
}

/// Mirrors `isSafeLinkUrl` in rich-html.ts.
pub fn is_safe_link_url(value: &str) -> bool {
    let compact = compact_url(value);
    if compact.starts_with('#') {
        return !compact.chars().any(char::is_whitespace);
    }
    match url::Url::parse(&compact) {
        Ok(url) => matches!(url.scheme(), "http" | "https" | "mailto" | "tel"),
        Err(_) => false,
    }
}

fn is_safe_inline_image(value: &str) -> bool {
    SAFE_INLINE_IMAGE.is_match(&compact_url(value))
}

fn filter_attribute<'u>(element: &str, attribute: &str, value: &'u str) -> Option<Cow<'u, str>> {
    match (element, attribute) {
        (_, "style") => (!has_dangerous_css(value)).then_some(Cow::Borrowed(value)),
        ("a", "href") => is_safe_link_url(value).then_some(Cow::Borrowed(value)),
        ("img", "src") => is_safe_inline_image(value).then_some(Cow::Borrowed(value)),
        _ => Some(Cow::Borrowed(value)),
    }
}

fn builder() -> ammonia::Builder<'static> {
    let mut b = ammonia::Builder::default();
    b.rm_tags(REMOVED_WITH_CONTENT.iter().copied())
        .add_tags(EXTRA_TAGS.iter().copied())
        .clean_content_tags(REMOVED_WITH_CONTENT.iter().copied().collect::<HashSet<_>>())
        .add_generic_attributes(EXTRA_GENERIC_ATTRIBUTES.iter().copied())
        .add_tag_attributes("font", ["color", "face", "size"].iter().copied())
        .add_tag_attributes("table", EXTRA_TABLE_ATTRIBUTES.iter().copied())
        .add_tag_attributes("td", EXTRA_TABLE_ATTRIBUTES.iter().copied())
        .add_tag_attributes("th", EXTRA_TABLE_ATTRIBUTES.iter().copied())
        .add_tag_attributes("tr", ["bgcolor", "valign"].iter().copied())
        .url_schemes(
            ["http", "https", "mailto", "tel", "cid", "data"]
                .into_iter()
                .collect(),
        )
        .url_relative(ammonia::UrlRelative::PassThrough)
        .link_rel(Some("noopener noreferrer"))
        .attribute_filter(filter_attribute);
    b
}

static BUILDER: LazyLock<ammonia::Builder<'static>> = LazyLock::new(builder);

/// Sanitize email HTML to body markup that cannot run, load, or navigate
/// anywhere unsafe.
pub fn sanitize_email_html(raw: &str) -> String {
    BUILDER.clean(raw).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Fixtures {
        cases: Vec<Case>,
    }

    #[derive(Deserialize)]
    struct Case {
        name: String,
        input: String,
        must_contain: Vec<String>,
        must_not_match: Vec<String>,
    }

    #[test]
    fn shared_fixtures_hold_for_the_server_sanitizer() {
        let fixtures: Fixtures = serde_json::from_str(include_str!(
            "../../dashboard/web/src/lib/components/sanitize-fixtures.json"
        ))
        .expect("fixture file parses");
        assert!(fixtures.cases.len() >= 7);
        for case in fixtures.cases {
            let html = sanitize_email_html(&case.input);
            for needle in &case.must_contain {
                assert!(
                    html.contains(needle.as_str()),
                    "{}: missing {needle:?} in {html}",
                    case.name
                );
            }
            for pattern in &case.must_not_match {
                let re = Regex::new(&format!("(?i){pattern}")).unwrap();
                assert!(
                    !re.is_match(&html),
                    "{}: {pattern:?} matched {html}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn link_policy_matches_the_reader() {
        assert!(is_safe_link_url("https://example.com/path"));
        assert!(is_safe_link_url("mailto:person@example.com"));
        assert!(is_safe_link_url("tel:+15551212"));
        assert!(is_safe_link_url("#section"));
        assert!(!is_safe_link_url("java\nscript:alert(1)"));
        assert!(!is_safe_link_url("vbscript:msgbox(1)"));
        assert!(!is_safe_link_url(
            "data:text/html,<script>alert(1)</script>"
        ));
        assert!(!is_safe_link_url("/relative/navigation"));
    }
}
