//! When the tray interrupts the user, decided without a window, a server or a
//! notification centre so every rule here is testable.

use std::collections::HashMap;

/// Accumulates Server-Sent Events lines into complete events.
///
/// Only the two fields the server sends are kept: `event:` names the type and
/// `data:` carries the JSON. Comment lines (`:keep-alive`) and unknown fields
/// are ignored, as the SSE spec requires.
#[derive(Debug, Default)]
pub struct SseParser {
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Feed one line without its terminator. Returns `(event, data)` when the
    /// blank line that ends an event arrives.
    pub fn push_line(&mut self, line: &str) -> Option<(String, String)> {
        let line = line.strip_suffix('\r').unwrap_or(line);

        if line.is_empty() {
            let data = std::mem::take(&mut self.data);
            let event = self.event.take();
            if data.is_empty() {
                return None;
            }
            return Some((event.unwrap_or_else(|| "message".into()), data.join("\n")));
        }

        if line.starts_with(':') {
            return None;
        }

        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }
}

/// The account and unread count from a `new_mail` event's data.
pub fn parse_new_mail(data: &str) -> Option<(String, usize)> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    let account_id = value.get("account_id")?.as_str()?.to_string();
    let unread = usize::try_from(value.get("unread_count")?.as_u64()?).ok()?;
    Some((account_id, unread))
}

/// Account id to address, from the dashboard's `GET /api/accounts` body.
pub fn parse_account_names(body: &str) -> HashMap<String, String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return HashMap::new();
    };
    value
        .get("accounts")
        .and_then(|accounts| accounts.as_array())
        .into_iter()
        .flatten()
        .filter_map(|account| {
            let id = account.get("id")?.as_str()?;
            let username = account.get("username")?.as_str()?;
            Some((id.to_string(), username.to_string()))
        })
        .collect()
}

/// Unread counts per account, so an alert fires only when unread mail grows.
///
/// The server reports a count after each sync, not individual arrivals. The
/// first count for an account is the baseline: alerting on it would announce
/// the whole existing backlog every time the app starts.
#[derive(Debug, Default)]
pub struct UnreadTracker {
    seen: HashMap<String, usize>,
}

impl UnreadTracker {
    /// How many more unread messages the account has than last time, if any.
    pub fn observe(&mut self, account_id: &str, unread: usize) -> Option<usize> {
        let previous = self.seen.insert(account_id.to_string(), unread)?;
        unread.checked_sub(previous).filter(|grew| *grew > 0)
    }
}

pub fn new_mail_text(count: usize, account: Option<&str>) -> String {
    let messages = if count == 1 {
        "1 new message".to_string()
    } else {
        format!("{count} new messages")
    };
    match account {
        Some(account) => format!("{messages} in {account}"),
        None => messages,
    }
}

/// The tray's one-line status for a supervisor readiness value.
pub fn status_line(readiness: &str) -> &'static str {
    match readiness {
        "dashboard_ready" => "Envelope is running",
        "server_stopped" => "Server stopped",
        "server_start_failed" => "Server couldn't start",
        "server_not_started" => "Server not running",
        _ => "Server not responding",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthAlert {
    Down,
    Recovered,
}

/// Watches the server across status polls and says when to tell the user.
///
/// One failed poll is not an outage: a probe can time out while the server is
/// busy syncing, so an alert waits for two in a row. A server the user stopped
/// is not an outage either.
#[derive(Debug, Default)]
pub struct HealthWatch {
    misses: u8,
    alerted: bool,
    seen_healthy: bool,
}

const MISSES_BEFORE_ALERT: u8 = 2;

impl HealthWatch {
    pub fn observe(&mut self, healthy: bool, stopped_by_user: bool) -> Option<HealthAlert> {
        if stopped_by_user {
            self.misses = 0;
            self.alerted = false;
            return None;
        }

        if healthy {
            self.misses = 0;
            self.seen_healthy = true;
            if self.alerted {
                self.alerted = false;
                return Some(HealthAlert::Recovered);
            }
            return None;
        }

        // A server that never came up already shows its failure screen in the
        // window; the alert is for one that was working and stopped.
        if !self.seen_healthy || self.alerted {
            return None;
        }

        self.misses = self.misses.saturating_add(1);
        if self.misses >= MISSES_BEFORE_ALERT {
            self.alerted = true;
            return Some(HealthAlert::Down);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(parser: &mut SseParser, text: &str) -> Vec<(String, String)> {
        text.split('\n')
            .filter_map(|line| parser.push_line(line))
            .collect()
    }

    #[test]
    fn sse_events_are_assembled_and_heartbeats_ignored() {
        let mut parser = SseParser::default();
        let events = feed(
            &mut parser,
            ":keep-alive\n\nevent: new_mail\ndata: {\"account_id\":\"a\",\"unread_count\":3}\n\nevent: account_health\r\ndata: {}\r\n\r\n",
        );
        assert_eq!(
            events,
            vec![
                (
                    "new_mail".to_string(),
                    "{\"account_id\":\"a\",\"unread_count\":3}".to_string()
                ),
                ("account_health".to_string(), "{}".to_string()),
            ]
        );
    }

    #[test]
    fn an_event_split_across_reads_is_not_lost() {
        let mut parser = SseParser::default();
        assert!(parser.push_line("event: new_mail").is_none());
        assert!(parser.push_line("data: {\"account_id\":\"a\",").is_none());
        assert!(parser.push_line("data: \"unread_count\":1}").is_none());
        let (event, data) = parser.push_line("").unwrap();
        assert_eq!(event, "new_mail");
        assert_eq!(parse_new_mail(&data), Some(("a".to_string(), 1)));
    }

    #[test]
    fn new_mail_needs_an_account_and_a_count() {
        assert_eq!(
            parse_new_mail(
                r#"{"type":"new_mail","account_id":"a","message_count":9,"unread_count":4}"#
            ),
            Some(("a".to_string(), 4))
        );
        assert_eq!(parse_new_mail(r#"{"account_id":"a"}"#), None);
        assert_eq!(parse_new_mail("not json"), None);
    }

    #[test]
    fn account_names_come_from_the_accounts_list() {
        let names = parse_account_names(
            r#"{"accounts":[{"id":"a1","username":"me@example.com"},{"id":"a2"}]}"#,
        );
        assert_eq!(names.get("a1").map(String::as_str), Some("me@example.com"));
        assert_eq!(names.len(), 1);
        assert!(parse_account_names("<html>").is_empty());
    }

    #[test]
    fn the_first_count_is_a_baseline_and_only_growth_alerts() {
        let mut tracker = UnreadTracker::default();
        assert_eq!(tracker.observe("a", 40), None, "existing backlog");
        assert_eq!(tracker.observe("a", 40), None, "no change");
        assert_eq!(tracker.observe("a", 43), Some(3));
        assert_eq!(tracker.observe("a", 41), None, "user read two");
        assert_eq!(tracker.observe("b", 2), None, "other account's baseline");
        assert_eq!(tracker.observe("a", 42), Some(1));
    }

    #[test]
    fn new_mail_text_names_the_account_when_known() {
        assert_eq!(
            new_mail_text(1, Some("me@example.com")),
            "1 new message in me@example.com"
        );
        assert_eq!(new_mail_text(3, None), "3 new messages");
    }

    #[test]
    fn every_readiness_has_a_plain_status_line() {
        assert_eq!(status_line("dashboard_ready"), "Envelope is running");
        assert_eq!(status_line("server_stopped"), "Server stopped");
        assert_eq!(status_line("api_route_failure"), "Server not responding");
    }

    #[test]
    fn one_slow_poll_is_not_an_outage() {
        let mut watch = HealthWatch::default();
        assert_eq!(watch.observe(true, false), None);
        assert_eq!(watch.observe(false, false), None);
        assert_eq!(watch.observe(true, false), None);
    }

    #[test]
    fn a_working_server_that_stops_alerts_once_then_recovers() {
        let mut watch = HealthWatch::default();
        watch.observe(true, false);
        assert_eq!(watch.observe(false, false), None);
        assert_eq!(watch.observe(false, false), Some(HealthAlert::Down));
        assert_eq!(
            watch.observe(false, false),
            None,
            "no repeat while still down"
        );
        assert_eq!(watch.observe(true, false), Some(HealthAlert::Recovered));
    }

    #[test]
    fn stopping_the_server_on_purpose_is_silent() {
        let mut watch = HealthWatch::default();
        watch.observe(true, false);
        for _ in 0..5 {
            assert_eq!(watch.observe(false, true), None);
        }
        assert_eq!(
            watch.observe(true, false),
            None,
            "starting it again is silent too"
        );
    }

    #[test]
    fn a_server_that_never_started_leaves_the_alert_to_the_window() {
        let mut watch = HealthWatch::default();
        for _ in 0..5 {
            assert_eq!(watch.observe(false, false), None);
        }
    }
}
