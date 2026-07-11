// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Result, bail};
use chrono::{
    DateTime, Datelike, Duration, Local, LocalResult, NaiveDateTime, NaiveTime, TimeZone, Utc,
    Weekday,
};

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

/// Parse a flexible datetime string into an ISO8601 datetime.
///
/// Accepts:
/// - ISO8601: "2026-03-30T09:00:00"
/// - Relative: "2h", "3d", "1w", "30m"
/// - Natural: "tomorrow", "monday", "tuesday", ..., "next week"
pub fn parse_until(input: &str) -> Result<String> {
    let trimmed = input.trim();
    let now = Local::now();

    // Try ISO8601 first — assume user means local time, convert to UTC
    if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S") {
        let local: Option<DateTime<Local>> = Local.from_local_datetime(&dt).single();
        if let Some(local_dt) = local {
            return Ok(local_dt
                .with_timezone(&Utc)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string());
        }
        return Ok(dt.format("%Y-%m-%dT%H:%M:%S").to_string());
    }

    // Try relative: number + unit suffix (relative to now, already UTC-safe)
    if trimmed.len() >= 2 {
        let (num_part, unit) = trimmed.split_at(trimmed.len() - 1);
        if let Ok(n) = num_part.parse::<i64>() {
            let duration = match unit {
                "m" => Duration::minutes(n),
                "h" => Duration::hours(n),
                "d" => Duration::days(n),
                "w" => Duration::weeks(n),
                _ => bail!("unknown time unit: '{unit}' (use m/h/d/w)"),
            };
            let target = Utc::now() + duration;
            return Ok(target.format("%Y-%m-%dT%H:%M:%S").to_string());
        }
    }

    // Try natural language
    let lower = trimmed.to_lowercase();
    let morning = NaiveTime::from_hms_opt(9, 0, 0).unwrap();

    // Helper: convert a local naive datetime to UTC string
    let to_utc = |naive: NaiveDateTime| -> String {
        let local_dt: Option<DateTime<Local>> = Local.from_local_datetime(&naive).single();
        match local_dt {
            Some(dt) => dt
                .with_timezone(&Utc)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string(),
            None => naive.format("%Y-%m-%dT%H:%M:%S").to_string(),
        }
    };

    match lower.as_str() {
        "tomorrow" => {
            let target = (now + Duration::days(1)).date_naive().and_time(morning);
            Ok(to_utc(target))
        }
        "next week" => {
            // Next Monday at 09:00
            let days_until_monday = (8 - now.weekday().num_days_from_monday()) % 7;
            let days = if days_until_monday == 0 {
                7
            } else {
                days_until_monday as i64
            };
            let target = (now + Duration::days(days)).date_naive().and_time(morning);
            Ok(to_utc(target))
        }
        day_name => {
            // Try parsing as a day of the week
            let target_weekday = match day_name {
                "monday" | "mon" => Some(Weekday::Mon),
                "tuesday" | "tue" => Some(Weekday::Tue),
                "wednesday" | "wed" => Some(Weekday::Wed),
                "thursday" | "thu" => Some(Weekday::Thu),
                "friday" | "fri" => Some(Weekday::Fri),
                "saturday" | "sat" => Some(Weekday::Sat),
                "sunday" | "sun" => Some(Weekday::Sun),
                _ => None,
            };

            if let Some(wd) = target_weekday {
                let current = now.weekday().num_days_from_monday();
                let target = wd.num_days_from_monday();
                let days = if target > current {
                    (target - current) as i64
                } else {
                    (7 - current + target) as i64
                };
                let target_dt = (now + Duration::days(days)).date_naive().and_time(morning);
                Ok(to_utc(target_dt))
            } else {
                bail!(
                    "cannot parse '{trimmed}' as a datetime. \
                     Use ISO8601 (2026-03-30T09:00:00), relative (2h, 3d, 1w), \
                     or natural (tomorrow, monday, next week)"
                )
            }
        }
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

    #[test]
    fn parse_iso8601() {
        // ISO8601 input is interpreted as local time, converted to UTC
        let result = parse_until("2026-03-30T09:00:00").unwrap();
        // Should be a valid datetime string (UTC-adjusted)
        assert!(NaiveDateTime::parse_from_str(&result, "%Y-%m-%dT%H:%M:%S").is_ok());
    }

    #[test]
    fn parse_relative_hours() {
        let result = parse_until("2h").unwrap();
        assert!(result.contains("T"));
    }

    #[test]
    fn parse_relative_days() {
        let result = parse_until("3d").unwrap();
        assert!(result.contains("T"));
    }

    #[test]
    fn parse_relative_weeks() {
        let result = parse_until("1w").unwrap();
        assert!(result.contains("T"));
    }

    #[test]
    fn parse_tomorrow() {
        let result = parse_until("tomorrow").unwrap();
        // 09:00 local converted to UTC — exact time depends on timezone
        assert!(NaiveDateTime::parse_from_str(&result, "%Y-%m-%dT%H:%M:%S").is_ok());
    }

    #[test]
    fn parse_day_name() {
        let result = parse_until("monday").unwrap();
        assert!(NaiveDateTime::parse_from_str(&result, "%Y-%m-%dT%H:%M:%S").is_ok());
    }

    #[test]
    fn parse_next_week() {
        let result = parse_until("next week").unwrap();
        assert!(NaiveDateTime::parse_from_str(&result, "%Y-%m-%dT%H:%M:%S").is_ok());
    }

    #[test]
    fn parse_invalid() {
        assert!(parse_until("banana").is_err());
    }
}
