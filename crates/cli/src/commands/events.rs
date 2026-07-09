// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use anyhow::{Context, Result, bail};
use envelope_email_store::Database;
use envelope_email_store::credential_store::CredentialBackend;
use envelope_email_store::event_deliveries::DeliveryStatusFilter;
use envelope_email_store::models::{Event, EventDelivery, EventRoute};
use envelope_email_transport::code_extractor::redact_codes;

use super::common::resolve_account;

/// Length of the route-secret prefix shown in list output. The full secret is
/// only ever printed once, at route creation.
const SECRET_PREFIX_LEN: usize = 12;

/// Redact a route secret to a recognizable prefix for list output.
fn redact_secret(secret: Option<&str>) -> String {
    match secret {
        Some(s) if s.len() > SECRET_PREFIX_LEN => {
            format!("{}…", &s[..SECRET_PREFIX_LEN])
        }
        Some(_) => "set".to_string(),
        None => "(none)".to_string(),
    }
}

pub fn run_list(
    account: Option<&str>,
    limit: usize,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let account_id = match account {
        Some(id_or_email) => Some(resolve_account(&db, Some(id_or_email))?.id),
        None => None,
    };

    let events = db
        .list_events(account_id.as_deref(), limit)
        .context("failed to list events")?
        .into_iter()
        .map(redact_event_for_output)
        .collect::<Vec<_>>();

    if json {
        println!("{}", serde_json::to_string_pretty(&events)?);
        return Ok(());
    }

    if events.is_empty() {
        println!("No events found");
        return Ok(());
    }

    println!(
        "{:<19}  {:<14}  {:<10}  {:<8}  {:<6}  {}",
        "CREATED", "TYPE", "ACCOUNT", "FOLDER", "ACKED", "SUBJECT"
    );
    println!("{}", "-".repeat(96));
    for event in &events {
        let created_at = truncate(&event.created_at, 19);
        let account = truncate(&event.account_id, 10);
        let folder = truncate(&event.folder, 8);
        let acked = if event.acked_at.is_some() {
            "yes"
        } else {
            "no"
        };
        let subject = event
            .subject
            .as_deref()
            .or(event.snippet.as_deref())
            .unwrap_or("-");
        println!(
            "{:<19}  {:<14}  {:<10}  {:<8}  {:<6}  {}",
            created_at,
            truncate(&event.event_type, 14),
            account,
            folder,
            acked,
            truncate(subject, 80)
        );
    }
    println!("\n{} event(s)", events.len());

    Ok(())
}

pub fn run_ack(
    event_id: &str,
    _actor: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    if !db
        .mark_acked(event_id)
        .context("failed to mark event acked")?
    {
        bail!("event not found: {event_id}");
    }

    let event = db
        .get_event(event_id)
        .context("failed to reload event")?
        .ok_or_else(|| anyhow::anyhow!("event not found after ack: {event_id}"))?;
    let event = redact_event_for_output(event);

    if json {
        println!("{}", serde_json::to_string_pretty(&event)?);
    } else {
        println!("Acked event {}", event.id);
        println!("  Type:    {}", event.event_type);
        println!("  Account: {}", event.account_id);
        println!("  Folder:  {}", event.folder);
        println!(
            "  Acked:   {}",
            event.acked_at.as_deref().unwrap_or("(unknown)")
        );
    }

    Ok(())
}

fn truncate(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_string();
    }
    value
        .chars()
        .take(max_len.saturating_sub(3))
        .collect::<String>()
        + "..."
}

fn redact_event_for_output(mut event: Event) -> Event {
    event.subject = event.subject.as_deref().map(redact_codes);
    event.snippet = event.snippet.as_deref().map(redact_codes);
    event.payload = event.payload.as_deref().map(redact_codes);
    event
}

// ── Event routes (durable delivery) ─────────────────────────────────

/// Build the `match_expr` JSON for a route from an optional comma-separated
/// event-type list. `None`/empty means match all events.
fn build_route_match_expr(event_types: Option<&str>) -> String {
    match event_types {
        Some(raw) if !raw.trim().is_empty() => {
            let types: Vec<&str> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            serde_json::json!({ "event_types": types }).to_string()
        }
        _ => serde_json::json!({}).to_string(),
    }
}

pub fn run_route_add(
    url: &str,
    event_types: Option<&str>,
    account: Option<&str>,
    priority: i64,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let account_id = resolve_account(&db, account)?.id;

    let match_expr = build_route_match_expr(event_types);
    let delivery = serde_json::json!({ "type": "webhook", "url": url }).to_string();
    let route = db
        .create_event_route(&account_id, &match_expr, &delivery, true, priority)
        .context("failed to create event route")?;

    // The secret is surfaced exactly once, here.
    let secret = route.secret.clone().unwrap_or_default();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "created",
                "route_id": route.id,
                "account_id": route.account_id,
                "url": url,
                "match_expr": route.match_expr,
                "priority": route.priority,
                "secret": secret,
                "secret_notice": "store this now; it is never shown again",
            })
        );
    } else {
        println!("Created event route {}", route.id);
        println!("  URL:      {url}");
        println!("  Match:    {}", route.match_expr);
        println!("  Priority: {}", route.priority);
        println!();
        println!("  Signing secret (shown ONCE — store it now):");
        println!("    {secret}");
        println!("  Requests carry X-Envelope-Signature: sha256=HMAC-SHA256(secret, body).");
    }
    Ok(())
}

pub fn run_route_list(
    account: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let account_id = resolve_account(&db, account)?.id;
    let routes = db
        .list_event_routes(&account_id)
        .context("failed to list event routes")?;

    if json {
        let redacted: Vec<serde_json::Value> = routes.iter().map(route_json_redacted).collect();
        println!("{}", serde_json::to_string_pretty(&redacted)?);
        return Ok(());
    }

    if routes.is_empty() {
        println!("No event routes");
        return Ok(());
    }

    println!(
        "{:<36}  {:<8}  {:<14}  DELIVERY",
        "ROUTE ID", "PRIORITY", "SECRET"
    );
    println!("{}", "-".repeat(96));
    for route in &routes {
        println!(
            "{:<36}  {:<8}  {:<14}  {}",
            route.id,
            route.priority,
            redact_secret(route.secret.as_deref()),
            truncate(&route.delivery, 40)
        );
    }
    println!("\n{} route(s)", routes.len());
    Ok(())
}

/// Route JSON for list output — never includes the raw secret.
fn route_json_redacted(route: &EventRoute) -> serde_json::Value {
    serde_json::json!({
        "id": route.id,
        "account_id": route.account_id,
        "match_expr": route.match_expr,
        "delivery": route.delivery,
        "enabled": route.enabled,
        "priority": route.priority,
        "secret_prefix": redact_secret(route.secret.as_deref()),
        "created_at": route.created_at,
        "updated_at": route.updated_at,
    })
}

pub fn run_route_remove(route_id: &str, json: bool, _backend: CredentialBackend) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let removed = db
        .delete_event_route(route_id)
        .context("failed to remove event route")?;
    if !removed {
        bail!("event route not found: {route_id}");
    }
    if json {
        println!(
            "{}",
            serde_json::json!({ "status": "removed", "route_id": route_id })
        );
    } else {
        println!("Removed event route {route_id}");
    }
    Ok(())
}

// ── Event deliveries ────────────────────────────────────────────────

fn parse_status_filter(status: &str) -> Result<DeliveryStatusFilter> {
    match status.to_ascii_lowercase().as_str() {
        "pending" => Ok(DeliveryStatusFilter::Pending),
        "dead" => Ok(DeliveryStatusFilter::Dead),
        "delivered" => Ok(DeliveryStatusFilter::Delivered),
        "all" => Ok(DeliveryStatusFilter::All),
        other => bail!("unknown status filter '{other}' (use pending|dead|delivered|all)"),
    }
}

pub fn run_delivery_list(
    status: &str,
    limit: usize,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let filter = parse_status_filter(status)?;
    let deliveries = db
        .list_deliveries(filter, limit)
        .context("failed to list deliveries")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&deliveries)?);
        return Ok(());
    }

    if deliveries.is_empty() {
        println!("No deliveries");
        return Ok(());
    }

    println!(
        "{:<36}  {:<10}  {:<8}  {:<6}  NEXT / RESULT",
        "DELIVERY ID", "STATUS", "ATTEMPTS", "CODE"
    );
    println!("{}", "-".repeat(96));
    for d in &deliveries {
        println!(
            "{:<36}  {:<10}  {:<8}  {:<6}  {}",
            d.id,
            delivery_status_label(d),
            d.attempt_count,
            d.last_status_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string()),
            delivery_detail(d),
        );
    }
    println!("\n{} delivery(ies)", deliveries.len());
    Ok(())
}

fn delivery_status_label(d: &EventDelivery) -> &'static str {
    if d.delivered_at.is_some() {
        "delivered"
    } else if d.dead_lettered_at.is_some() {
        "dead"
    } else {
        "pending"
    }
}

fn delivery_detail(d: &EventDelivery) -> String {
    if d.delivered_at.is_some() {
        format!("delivered {}", d.delivered_at.as_deref().unwrap_or("-"))
    } else if d.dead_lettered_at.is_some() {
        format!(
            "dead-lettered: {}",
            d.last_error.as_deref().unwrap_or("(no error)")
        )
    } else if let Some(next) = &d.next_attempt_at {
        format!("retry at {next}")
    } else {
        "due".to_string()
    }
}

pub fn run_delivery_retry(
    delivery_id: &str,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let reset = db
        .reset_delivery_for_retry(delivery_id)
        .context("failed to retry delivery")?;
    if !reset {
        bail!("delivery not found or already delivered: {delivery_id}");
    }
    if json {
        println!(
            "{}",
            serde_json::json!({ "status": "retry_scheduled", "delivery_id": delivery_id })
        );
    } else {
        println!(
            "Cleared dead-letter/backoff for delivery {delivery_id}; it will be retried on the next executor pass"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_redaction_scrubs_legacy_unredacted_event_rows() {
        let event = Event {
            id: "evt-legacy".to_string(),
            account_id: "acc-1".to_string(),
            event_type: "otp_detected".to_string(),
            folder: "INBOX".to_string(),
            uid: Some(42),
            message_id: Some("<msg@example.com>".to_string()),
            from_addr: Some("noreply@example.com".to_string()),
            subject: Some("Your code is 482910".to_string()),
            snippet: Some("Use 482910 or 482-910 to sign in".to_string()),
            payload: Some(r#"{"debug":"code 482910"}"#.to_string()),
            idempotency_key: Some("same-key".to_string()),
            secure_pending: true,
            acked_at: None,
            created_at: "2026-04-25T12:00:00".to_string(),
        };

        let redacted = redact_event_for_output(event);
        let serialized = serde_json::to_string(&redacted).unwrap();
        assert!(!serialized.contains("482910"));
        assert!(!serialized.contains("482-910"));
        assert_eq!(redacted.subject.as_deref(), Some("Your code is ***"));
    }
}
