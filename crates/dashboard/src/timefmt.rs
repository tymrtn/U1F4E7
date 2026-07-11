// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Canonical UTC timestamp handling for the dashboard.
//!
//! Everything the dashboard queues, sweeps, or compares (draft `send_after`,
//! snooze `return_at`, `generated_at`, countdowns) lives in one frame: UTC.
//! SQLite's `datetime('now')` — used by the due-for-send query — is UTC, and
//! every writer stores naive-UTC or RFC 3339 `Z` strings. Reading wall-clock
//! time as `chrono::Local` here silently skews every due comparison and
//! countdown by the host's UTC offset, so this module is the only place the
//! dashboard asks for "now".
//!
//! New values are written as RFC 3339 UTC with a `Z` suffix; parsing accepts
//! both that and the legacy naive `%Y-%m-%dT%H:%M:%S` rows (interpreted as
//! UTC) that existing databases already hold.

use chrono::{DateTime, NaiveDateTime, Utc};

/// Current UTC time as RFC 3339 with a `Z` suffix (second precision).
pub fn utc_now_string() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Parse a stored timestamp into UTC. Accepts RFC 3339 (with `Z` or a numeric
/// offset) and legacy naive `%Y-%m-%dT%H:%M:%S` values, which are UTC by
/// construction (all queue writers use `chrono::Utc`). Returns `None` for
/// anything else.
pub fn parse_utc(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|n| n.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_utc_reads_legacy_naive_rows_as_utc() {
        let naive = parse_utc("2026-07-08T09:00:00").unwrap();
        let rfc3339 = parse_utc("2026-07-08T09:00:00Z").unwrap();
        assert_eq!(naive, rfc3339, "naive rows are UTC, not local");
    }

    #[test]
    fn parse_utc_normalizes_offsets() {
        let offset = parse_utc("2026-07-08T11:00:00+02:00").unwrap();
        let utc = parse_utc("2026-07-08T09:00:00Z").unwrap();
        assert_eq!(offset, utc);
    }

    #[test]
    fn parse_utc_rejects_garbage() {
        assert!(parse_utc("not a time").is_none());
        assert!(parse_utc("").is_none());
    }

    #[test]
    fn utc_now_string_is_rfc3339_utc() {
        let now = utc_now_string();
        assert!(now.ends_with('Z'), "canonical now must carry the Z suffix");
        let parsed = parse_utc(&now).expect("canonical now must round-trip");
        // Regression guard for the Local-vs-UTC skew: the string must denote
        // the actual UTC instant, not local wall-clock relabeled as UTC. On a
        // non-UTC host a chrono::Local regression puts this whole hours off.
        let delta = (Utc::now() - parsed).num_seconds().abs();
        assert!(delta < 5, "utc_now_string is {delta}s away from Utc::now()");
    }
}
