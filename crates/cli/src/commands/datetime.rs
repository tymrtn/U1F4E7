// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Result, bail};
use chrono::{
    DateTime, Datelike, Duration, Local, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday,
};

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
