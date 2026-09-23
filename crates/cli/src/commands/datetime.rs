// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Result, bail};
use chrono::{DateTime, Local, LocalResult, NaiveDateTime, TimeZone, Utc};

pub use envelope_email_transport::snooze_time::parse_until;

/// Parse a scheduled-send `--at` value into **canonical RFC 3339 UTC (`Z`)**.
///
/// Accepts:
/// - RFC 3339 with an explicit offset: "2026-03-30T09:00:00+02:00" / "…Z"
/// - Naive ISO 8601 ("2026-03-30T09:00:00"), interpreted as LOCAL time —
///   strictly: an ambiguous local time (DST fall-back repeat) or a
///   nonexistent one (spring-forward gap) is REJECTED with instructions to
///   supply an explicit offset, never silently relabeled as UTC.
/// - Relative ("2h", "3d", "1w", "30m") and natural ("tomorrow", "monday",
///   "next week") forms, via the same strict local conversion.
pub fn parse_send_at(input: &str) -> Result<String> {
    let trimmed = input.trim();

    // Explicit offset wins: unambiguous by construction.
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt
            .with_timezone(&Utc)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string());
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S") {
        return Ok(
            resolve_local_result(Local.from_local_datetime(&dt), trimmed)?
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    // Relative and natural forms share parse_until's UTC frame; re-emit with
    // the canonical Z suffix. (Relative forms are computed from Utc::now();
    // natural forms are 09:00 local, where DST edge times cannot occur.)
    let naive_utc = parse_until(trimmed)?;
    let parsed = NaiveDateTime::parse_from_str(&naive_utc, "%Y-%m-%dT%H:%M:%S")
        .map_err(|e| anyhow::anyhow!("internal --at parse error for {naive_utc:?}: {e}"))?;
    Ok(parsed.and_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Strictly resolve a local-time interpretation: ambiguous (DST fall-back)
/// and nonexistent (spring-forward gap) wall-clock times are rejected with an
/// actionable message instead of being silently relabeled as UTC.
fn resolve_local_result(res: LocalResult<DateTime<Local>>, raw: &str) -> Result<DateTime<Utc>> {
    match res {
        LocalResult::Single(dt) => Ok(dt.with_timezone(&Utc)),
        LocalResult::Ambiguous(earlier, later) => bail!(
            "local time '{raw}' is ambiguous (DST fall-back: it occurs twice, as {} and {}). \
             Specify an explicit offset, e.g. '{raw}{}'",
            earlier.to_rfc3339(),
            later.to_rfc3339(),
            earlier.format("%:z")
        ),
        LocalResult::None => bail!(
            "local time '{raw}' does not exist in this timezone (DST spring-forward gap). \
             Pick a time outside the gap or specify an explicit offset (RFC 3339)."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_send_at (canonical RFC 3339 Z, strict local resolution) ──

    #[test]
    fn send_at_rfc3339_offset_input_normalizes_to_utc_z_deterministically() {
        // No local timezone involved: explicit offsets are exact on any host.
        assert_eq!(
            parse_send_at("2026-03-30T09:00:00+02:00").unwrap(),
            "2026-03-30T07:00:00Z"
        );
        assert_eq!(
            parse_send_at("2026-03-30T07:00:00Z").unwrap(),
            "2026-03-30T07:00:00Z"
        );
        assert_eq!(
            parse_send_at("2026-03-30T04:30:00-05:00").unwrap(),
            "2026-03-30T09:30:00Z"
        );
    }

    #[test]
    fn send_at_relative_and_naive_forms_emit_z_suffix() {
        assert!(parse_send_at("2h").unwrap().ends_with('Z'));
        assert!(parse_send_at("tomorrow").unwrap().ends_with('Z'));
        // A mid-day naive time is unambiguous in every real timezone.
        assert!(parse_send_at("2026-06-15T12:00:00").unwrap().ends_with('Z'));
    }

    #[test]
    fn strict_local_resolution_rejects_ambiguous_and_nonexistent_times() {
        // Deterministic: LocalResult variants are constructed directly, so the
        // rejection logic is exercised regardless of the host timezone.
        let instant = Local.from_utc_datetime(
            &NaiveDateTime::parse_from_str("2026-10-25T00:30:00", "%Y-%m-%dT%H:%M:%S").unwrap(),
        );
        let later = Local.from_utc_datetime(
            &NaiveDateTime::parse_from_str("2026-10-25T01:30:00", "%Y-%m-%dT%H:%M:%S").unwrap(),
        );

        let ok = resolve_local_result(LocalResult::Single(instant), "x").unwrap();
        assert_eq!(ok, instant.with_timezone(&Utc));

        let ambiguous = resolve_local_result(
            LocalResult::Ambiguous(instant, later),
            "2026-10-25T02:30:00",
        )
        .unwrap_err()
        .to_string();
        assert!(ambiguous.contains("ambiguous"), "{ambiguous}");
        assert!(
            ambiguous.contains("explicit offset"),
            "rejection must be actionable: {ambiguous}"
        );

        let nonexistent = resolve_local_result(LocalResult::None, "2026-03-29T02:30:00")
            .unwrap_err()
            .to_string();
        assert!(nonexistent.contains("does not exist"), "{nonexistent}");
    }
}
