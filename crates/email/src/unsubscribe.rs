// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! List-Unsubscribe header parsing and execution.
//!
//! Supports RFC 2369 `List-Unsubscribe` and RFC 8058 one-click
//! `List-Unsubscribe-Post`. Security: default is dry-run, execution
//! requires explicit `--confirm`. Never auto-follows GET URLs (tracking).

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::errors::SmtpError;

/// Parsed unsubscribe options from a message's headers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsubscribeInfo {
    /// HTTPS URLs from List-Unsubscribe header.
    pub https_urls: Vec<String>,
    /// Mailto addresses from List-Unsubscribe header.
    pub mailto_urls: Vec<String>,
    /// Whether RFC 8058 one-click POST is supported.
    pub one_click_post: bool,
    /// Raw List-Unsubscribe header value.
    pub raw_header: String,
}

/// Result of an unsubscribe attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsubscribeResult {
    pub method: String, // "https_post", "mailto", "none"
    pub url: Option<String>,
    pub status: String, // "success", "failed", "dry_run"
    pub message: String,
}

/// Parse the List-Unsubscribe header value.
///
/// Format: `<mailto:unsub@example.com>, <https://example.com/unsub?id=123>`
/// Returns None if the header is empty or missing.
pub fn parse_list_unsubscribe(header: &str, post_header: Option<&str>) -> Option<UnsubscribeInfo> {
    if header.trim().is_empty() {
        return None;
    }

    let mut https_urls = Vec::new();
    let mut mailto_urls = Vec::new();

    // Split on commas, extract angle-bracketed URLs
    for part in header.split(',') {
        let trimmed = part.trim();
        let url = trimmed.trim_start_matches('<').trim_end_matches('>').trim();
        if url.starts_with("https://") || url.starts_with("http://") {
            https_urls.push(url.to_string());
        } else if url.starts_with("mailto:") {
            mailto_urls.push(url.to_string());
        }
    }

    if https_urls.is_empty() && mailto_urls.is_empty() {
        return None;
    }

    let one_click_post = post_header
        .map(|h| h.to_lowercase().contains("list-unsubscribe=one-click"))
        .unwrap_or(false);

    Some(UnsubscribeInfo {
        https_urls,
        mailto_urls,
        one_click_post,
        raw_header: header.to_string(),
    })
}

/// Execute an unsubscribe action.
///
/// Priority: HTTPS POST (RFC 8058) → mailto → error.
/// Never uses GET (tracking risk).
///
/// If `confirm` is false, returns a dry-run result showing what would happen.
pub async fn execute_unsubscribe(
    info: &UnsubscribeInfo,
    confirm: bool,
    smtp_send: Option<&dyn Fn(&str) -> Result<(), SmtpError>>,
) -> UnsubscribeResult {
    // Prefer HTTPS POST (RFC 8058 one-click)
    if info.one_click_post {
        if let Some(url) = info.https_urls.first() {
            if !confirm {
                return UnsubscribeResult {
                    method: "https_post".to_string(),
                    url: Some(url.clone()),
                    status: "dry_run".to_string(),
                    message: format!("Would POST to {url} with List-Unsubscribe=One-Click"),
                };
            }

            debug!("unsubscribing via HTTPS POST: {url}");
            match reqwest::Client::new()
                .post(url)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body("List-Unsubscribe=One-Click")
                .send()
                .await
            {
                Ok(resp) => {
                    let status_code = resp.status();
                    if status_code.is_success() || status_code.as_u16() == 302 {
                        info!("unsubscribed via HTTPS POST: {url} → {status_code}");
                        return UnsubscribeResult {
                            method: "https_post".to_string(),
                            url: Some(url.clone()),
                            status: "success".to_string(),
                            message: format!("Unsubscribed via POST ({status_code})"),
                        };
                    } else {
                        warn!("HTTPS POST returned {status_code} for {url}");
                    }
                }
                Err(e) => {
                    warn!("HTTPS POST failed for {url}: {e}");
                }
            }
        }
    }

    // Fall back to HTTPS POST without one-click header (some providers accept it)
    if let Some(url) = info.https_urls.first() {
        if !info.one_click_post {
            if !confirm {
                return UnsubscribeResult {
                    method: "https_post".to_string(),
                    url: Some(url.clone()),
                    status: "dry_run".to_string(),
                    message: format!("Would POST to {url} (no one-click header, may not work)"),
                };
            }
        }
    }

    // Fall back to mailto
    if let Some(mailto) = info.mailto_urls.first() {
        let addr = mailto.trim_start_matches("mailto:");
        if !confirm {
            return UnsubscribeResult {
                method: "mailto".to_string(),
                url: Some(mailto.clone()),
                status: "dry_run".to_string(),
                message: format!("Would send unsubscribe email to {addr}"),
            };
        }

        // If we have an SMTP sender callback, use it
        if let Some(send_fn) = smtp_send {
            match send_fn(addr) {
                Ok(()) => {
                    info!("unsubscribed via mailto: {addr}");
                    return UnsubscribeResult {
                        method: "mailto".to_string(),
                        url: Some(mailto.clone()),
                        status: "success".to_string(),
                        message: format!("Sent unsubscribe email to {addr}"),
                    };
                }
                Err(e) => {
                    warn!("mailto unsubscribe failed for {addr}: {e}");
                    return UnsubscribeResult {
                        method: "mailto".to_string(),
                        url: Some(mailto.clone()),
                        status: "failed".to_string(),
                        message: format!("Failed to send to {addr}: {e}"),
                    };
                }
            }
        }

        return UnsubscribeResult {
            method: "mailto".to_string(),
            url: Some(mailto.clone()),
            status: "failed".to_string(),
            message: "No SMTP sender available for mailto unsubscribe".to_string(),
        };
    }

    UnsubscribeResult {
        method: "none".to_string(),
        url: None,
        status: "failed".to_string(),
        message: "No usable unsubscribe method found in headers".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_and_mailto() {
        let header = "<mailto:unsub@example.com>, <https://example.com/unsub?id=123>";
        let info = parse_list_unsubscribe(header, None).unwrap();
        assert_eq!(info.https_urls, vec!["https://example.com/unsub?id=123"]);
        assert_eq!(info.mailto_urls, vec!["mailto:unsub@example.com"]);
        assert!(!info.one_click_post);
    }

    #[test]
    fn parse_one_click_post() {
        let header = "<https://example.com/unsub>";
        let post = "List-Unsubscribe=One-Click";
        let info = parse_list_unsubscribe(header, Some(post)).unwrap();
        assert!(info.one_click_post);
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_list_unsubscribe("", None).is_none());
        assert!(parse_list_unsubscribe("   ", None).is_none());
    }

    #[test]
    fn parse_mailto_only() {
        let header = "<mailto:leave@lists.example.com>";
        let info = parse_list_unsubscribe(header, None).unwrap();
        assert!(info.https_urls.is_empty());
        assert_eq!(info.mailto_urls.len(), 1);
    }

    #[tokio::test]
    async fn dry_run_prefers_https_post() {
        let info = UnsubscribeInfo {
            https_urls: vec!["https://example.com/unsub".to_string()],
            mailto_urls: vec!["mailto:unsub@example.com".to_string()],
            one_click_post: true,
            raw_header: "test".to_string(),
        };
        let result = execute_unsubscribe(&info, false, None).await;
        assert_eq!(result.method, "https_post");
        assert_eq!(result.status, "dry_run");
    }

    #[tokio::test]
    async fn dry_run_falls_back_to_mailto() {
        let info = UnsubscribeInfo {
            https_urls: vec![],
            mailto_urls: vec!["mailto:unsub@example.com".to_string()],
            one_click_post: false,
            raw_header: "test".to_string(),
        };
        let result = execute_unsubscribe(&info, false, None).await;
        assert_eq!(result.method, "mailto");
        assert_eq!(result.status, "dry_run");
    }

    #[tokio::test]
    async fn no_methods_returns_none() {
        let info = UnsubscribeInfo {
            https_urls: vec![],
            mailto_urls: vec![],
            one_click_post: false,
            raw_header: "test".to_string(),
        };
        let result = execute_unsubscribe(&info, true, None).await;
        assert_eq!(result.method, "none");
        assert_eq!(result.status, "failed");
    }
}
