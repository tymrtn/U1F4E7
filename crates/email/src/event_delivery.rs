// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Durable event-delivery executor (v2 webhook push).
//!
//! [`deliver_due_events`] is the callable library core of the pipeline. Both
//! `envelope watch --deliver` and the dashboard sweep drive it: pick the due,
//! not-yet-delivered, not-dead-lettered delivery rows, POST each event body to
//! its route's webhook URL with a signed header set, capture the response, and
//! advance the row through an exponential-backoff retry schedule until it is
//! delivered or dead-lettered.
//!
//! # Signing
//! Each request carries `X-Envelope-Signature: sha256=<hex HMAC-SHA256>` where
//! the key is the route's per-route secret (minted once at route creation) and
//! the message is the exact JSON body sent. Receivers verify by recomputing the
//! HMAC over the raw body. The secret is never logged.
//!
//! # Backoff
//! Failure `n` (1-indexed) schedules the next attempt [`BACKOFF_SCHEDULE`]`[n-1]`
//! seconds out: 1m, 5m, 30m, 2h, 12h. There are [`MAX_ATTEMPTS`] (5) backed-off
//! retries; once all are consumed (the 6th failure overall) the delivery is
//! dead-lettered and never retried automatically. An operator can
//! `envelope events deliveries retry <id>` to clear the dead-letter.

use std::time::Duration;

use chrono::{DateTime, Utc};
use envelope_email_store::event_deliveries::cap_snippet;
use envelope_email_store::models::EventRoute;
use envelope_email_store::{Database, StoreError};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::warn;

/// Exponential backoff schedule in seconds: 1m, 5m, 30m, 2h, 12h. Its length is
/// [`MAX_ATTEMPTS`]; after the last entry the delivery is dead-lettered.
pub const BACKOFF_SCHEDULE: [i64; 5] = [60, 300, 1800, 7200, 43200];

/// Number of backed-off retries before dead-lettering. Equals the backoff
/// length; the initial attempt plus these retries means up to
/// `MAX_ATTEMPTS + 1` total POSTs before a delivery is given up on.
pub const MAX_ATTEMPTS: u32 = BACKOFF_SCHEDULE.len() as u32;

/// Per-request timeout for a webhook POST.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Tunable limits for one executor pass.
#[derive(Debug, Clone, Copy)]
pub struct DeliveryLimits {
    /// Maximum number of due deliveries to process in this pass.
    pub max_deliveries: usize,
}

impl Default for DeliveryLimits {
    fn default() -> Self {
        Self {
            max_deliveries: 100,
        }
    }
}

/// Outcome of one [`deliver_due_events`] pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeliveryReport {
    /// Deliveries examined (due rows fetched).
    pub examined: usize,
    /// Deliveries that got a 2xx response this pass.
    pub delivered: usize,
    /// Deliveries that failed and were rescheduled for a later retry.
    pub retried: usize,
    /// Deliveries that exhausted retries and were dead-lettered this pass.
    pub dead_lettered: usize,
    /// Deliveries skipped because their route or event no longer exists.
    pub skipped: usize,
}

/// The webhook delivery spec stored in `event_routes.delivery`.
#[derive(Debug, Deserialize)]
struct WebhookDelivery {
    #[serde(rename = "type")]
    kind: String,
    url: String,
}

/// Process all due deliveries, POSTing each to its route's webhook.
///
/// `now` is injected so callers (and tests) control the clock. Errors from
/// individual deliveries are recorded on the row and never abort the pass; only
/// a database-level failure propagates.
pub async fn deliver_due_events(
    db: &Database,
    http: &reqwest::Client,
    now: DateTime<Utc>,
    limits: DeliveryLimits,
) -> Result<DeliveryReport, StoreError> {
    let now_str = now.to_rfc3339();
    let due = db.list_due_deliveries(&now_str, limits.max_deliveries)?;

    let mut report = DeliveryReport {
        examined: due.len(),
        ..Default::default()
    };

    for delivery in due {
        // Resolve the route (secret + URL) and the event body.
        let route = match db.get_event_route(&delivery.route_id) {
            Ok(route) => route,
            Err(_) => {
                // Route was deleted after enqueue: dead-letter with a clear
                // reason so it does not spin forever.
                db.record_delivery_failure(
                    &delivery.id,
                    None,
                    None,
                    Some("route no longer exists"),
                    None,
                    &now_str,
                )?;
                report.skipped += 1;
                report.dead_lettered += 1;
                continue;
            }
        };

        let Some(event) = db.get_event(&delivery.event_id)? else {
            db.record_delivery_failure(
                &delivery.id,
                None,
                None,
                Some("event no longer exists"),
                None,
                &now_str,
            )?;
            report.skipped += 1;
            report.dead_lettered += 1;
            continue;
        };

        let webhook = match parse_webhook(&route) {
            Some(w) => w,
            None => {
                db.record_delivery_failure(
                    &delivery.id,
                    None,
                    None,
                    Some("route delivery is not a webhook target"),
                    None,
                    &now_str,
                )?;
                report.skipped += 1;
                report.dead_lettered += 1;
                continue;
            }
        };

        // Serialize the event body once; this exact byte string is both sent
        // and signed.
        let body = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
        let signature = route.secret.as_deref().map(|secret| {
            format!(
                "sha256={}",
                hmac_sha256_hex(secret.as_bytes(), body.as_bytes())
            )
        });

        // Warn operators about unsigned routes. Routes minted before migration 10
        // may have a NULL secret; without a signing secret the receiver cannot
        // verify the webhook origin. The delivery proceeds (fail-open by design —
        // stopping delivery would be worse than an unverified push for legacy
        // routes), but the operator should rotate to a new route with
        // `envelope events routes add` to get a signing secret.
        if route.secret.is_none() {
            warn!(
                route_id = %delivery.route_id,
                "event route has no signing secret — webhook receiver cannot \
                 verify origin; create a new route to obtain an HMAC secret"
            );
        }

        let attempt_result = post_webhook(
            http,
            &webhook.url,
            &event.event_type,
            &delivery.id,
            signature.as_deref(),
            &body,
        )
        .await;

        match attempt_result {
            AttemptOutcome::Success { status, snippet } => {
                db.record_delivery_success(&delivery.id, status, Some(&snippet), &now_str)?;
                report.delivered += 1;
            }
            AttemptOutcome::Failure {
                status,
                snippet,
                error,
            } => {
                // attempt_count is pre-increment; this is failure number N+1.
                let attempts_after = (delivery.attempt_count as u32) + 1;
                let next = next_attempt_at(attempts_after, now);
                if next.is_none() {
                    report.dead_lettered += 1;
                } else {
                    report.retried += 1;
                }
                db.record_delivery_failure(
                    &delivery.id,
                    status,
                    snippet.as_deref(),
                    Some(&error),
                    next.as_deref(),
                    &now_str,
                )?;
            }
        }
    }

    Ok(report)
}

/// Compute the next attempt timestamp after `attempts_after` total failures
/// (1-indexed: `attempts_after == 1` is the first failure). Failure `k`
/// schedules the next attempt [`BACKOFF_SCHEDULE`]`[k - 1]` seconds out. Once
/// all [`MAX_ATTEMPTS`] retry delays are consumed, returns `None` (dead-letter).
fn next_attempt_at(attempts_after: u32, now: DateTime<Utc>) -> Option<String> {
    if attempts_after == 0 || attempts_after > MAX_ATTEMPTS {
        return None;
    }
    let delay = BACKOFF_SCHEDULE[(attempts_after - 1) as usize];
    Some((now + chrono::Duration::seconds(delay)).to_rfc3339())
}

enum AttemptOutcome {
    Success {
        status: u16,
        snippet: String,
    },
    Failure {
        status: Option<u16>,
        snippet: Option<String>,
        error: String,
    },
}

async fn post_webhook(
    http: &reqwest::Client,
    url: &str,
    event_type: &str,
    delivery_id: &str,
    signature: Option<&str>,
    body: &str,
) -> AttemptOutcome {
    let mut req = http
        .post(url)
        .timeout(REQUEST_TIMEOUT)
        .header("Content-Type", "application/json")
        .header("X-Envelope-Event", event_type)
        .header("X-Envelope-Delivery", delivery_id);
    if let Some(sig) = signature {
        req = req.header("X-Envelope-Signature", sig);
    }

    match req.body(body.to_string()).send().await {
        Ok(resp) => {
            let status = resp.status();
            let code = status.as_u16();
            let text = resp.text().await.unwrap_or_default();
            let snippet = cap_snippet(&text);
            if status.is_success() {
                AttemptOutcome::Success {
                    status: code,
                    snippet,
                }
            } else {
                AttemptOutcome::Failure {
                    status: Some(code),
                    snippet: Some(snippet),
                    error: format!("non-2xx status {code}"),
                }
            }
        }
        Err(e) => {
            // reqwest error strings never include the request body or headers,
            // so this cannot leak the signing secret.
            warn!("webhook delivery transport error");
            AttemptOutcome::Failure {
                status: None,
                snippet: None,
                error: transport_error_summary(&e),
            }
        }
    }
}

fn parse_webhook(route: &EventRoute) -> Option<WebhookDelivery> {
    let parsed: WebhookDelivery = serde_json::from_str(&route.delivery).ok()?;
    if parsed.kind != "webhook" || parsed.url.is_empty() {
        return None;
    }
    Some(parsed)
}

fn transport_error_summary(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "request timed out".to_string()
    } else if e.is_connect() {
        "connection failed".to_string()
    } else {
        "transport error".to_string()
    }
}

/// HMAC-SHA256 over `message` keyed by `key`, hex-encoded. Implemented against
/// `sha2` directly (the standard ipad/opad construction) so no HMAC dependency
/// is added to the workspace.
pub fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut block_key = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        block_key[..digest.len()].copy_from_slice(&digest);
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= block_key[i];
        opad[i] ^= block_key[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    let mac = outer.finalize();

    let mut hex = String::with_capacity(mac.len() * 2);
    for byte in mac {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn hmac_matches_known_rfc4231_vector() {
        // RFC 4231 Test Case 2: key = "Jefe", data = "what do ya want for
        // nothing?", expected HMAC-SHA256 is a published constant.
        let mac = hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            mac,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_handles_key_longer_than_block() {
        // 100-byte key exercises the key-hashing branch; just assert stability.
        let key = vec![0xaau8; 100];
        let a = hmac_sha256_hex(&key, b"payload");
        let b = hmac_sha256_hex(&key, b"payload");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn backoff_schedule_dead_letters_after_max_attempts() {
        let now = Utc::now();
        // Failures 1..=MAX each schedule a backed-off retry.
        for n in 1..=MAX_ATTEMPTS {
            assert!(
                next_attempt_at(n, now).is_some(),
                "failure {n} should reschedule"
            );
        }
        // The failure after the last retry exhausts the schedule → dead-letter.
        assert!(next_attempt_at(MAX_ATTEMPTS + 1, now).is_none());
    }

    #[test]
    fn first_failure_uses_shortest_backoff() {
        let now = Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap();
        let next = next_attempt_at(1, now).unwrap();
        let parsed = DateTime::parse_from_rfc3339(&next).unwrap();
        assert_eq!(
            parsed.with_timezone(&Utc),
            now + chrono::Duration::seconds(BACKOFF_SCHEDULE[0])
        );
    }

    #[test]
    fn snippet_cap_constant_is_one_kib() {
        assert_eq!(
            envelope_email_store::event_deliveries::RESPONSE_SNIPPET_CAP_BYTES,
            1024
        );
    }

    // ── Unsigned-route guard ─────────────────────────────────────────

    /// Routes that carry a secret must produce a signature; routes without one
    /// must not (the warning fires at the call-site but we validate the
    /// output contract here).
    #[test]
    fn signature_present_iff_secret_present() {
        // Simulate what deliver_due_events does: build the signature if and
        // only if the route has a secret.
        let make_sig = |secret: Option<&str>, body: &str| -> Option<String> {
            secret.map(|s| format!("sha256={}", hmac_sha256_hex(s.as_bytes(), body.as_bytes())))
        };

        let body = r#"{"event_type":"new_message"}"#;

        let with_secret = make_sig(Some("evrt_abc123"), body);
        assert!(
            with_secret.is_some(),
            "secret present → signature must be produced"
        );
        assert!(
            with_secret.as_deref().unwrap().starts_with("sha256="),
            "signature must be in sha256=<hex> format"
        );

        let no_secret = make_sig(None, body);
        assert!(
            no_secret.is_none(),
            "no secret → no signature header must be sent"
        );
    }

    /// The signature produced with a known key must be stable across calls
    /// (deterministic HMAC, not a random value) so receivers can re-verify.
    #[test]
    fn signature_is_deterministic() {
        let body = r#"{"event_type":"otp_detected","id":"evt-1"}"#;
        let secret = "evrt_deterministic_test_key";
        let s1 = hmac_sha256_hex(secret.as_bytes(), body.as_bytes());
        let s2 = hmac_sha256_hex(secret.as_bytes(), body.as_bytes());
        assert_eq!(s1, s2);
        // Also verify it changes when the body changes (not a constant).
        let s3 = hmac_sha256_hex(secret.as_bytes(), b"different body");
        assert_ne!(s1, s3);
    }
}
