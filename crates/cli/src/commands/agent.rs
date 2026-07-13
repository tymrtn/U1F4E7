// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! `envelope agent` command group: per-agent identity + policy management.
//!
//! Creates bearer tokens (shown exactly once), lists/shows/revokes agents, and
//! sets/shows per-agent authorization policies. Creating more than the free-tier
//! allowance (2 active agents) requires an activated license.

use anyhow::{Context, Result, bail};
use envelope_email_store::{
    AgentPolicy as StoreAgentPolicy, Database, SendModeCeiling, credential_store::CredentialBackend,
};
use serde_json::json;

/// Free tier: up to this many active (non-revoked) agents without a license.
pub const FREE_TIER_AGENT_LIMIT: i64 = 2;

/// Stable status code returned when an unlicensed operator tries to exceed the
/// free-tier active-agent limit.
pub const AGENT_LIMIT_CODE: &str = "agent_limit_license_required";

// ── create ──────────────────────────────────────────────────────────

pub fn run_create(name: &str, json: bool, _backend: CredentialBackend) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;

    // License gate: honor-system. Any valid (non-expired) stored activation lifts
    // the free-tier cap. See license activation below the cap for details.
    let active = db
        .count_active_agents()
        .context("failed to count active agents")?;
    if active >= FREE_TIER_AGENT_LIMIT {
        let licensed = db
            .get_active_license()
            .context("failed to read license")?
            .is_some();
        if !licensed {
            let payload = json!({
                "status": "denied",
                "error": {
                    "code": AGENT_LIMIT_CODE,
                    "reason": format!(
                        "the free tier allows up to {FREE_TIER_AGENT_LIMIT} active agents; \
                         run `envelope license activate` to add more"
                    ),
                },
                "active_agents": active,
                "free_tier_limit": FREE_TIER_AGENT_LIMIT,
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                eprintln!(
                    "Free tier allows up to {FREE_TIER_AGENT_LIMIT} active agents \
                     ({active} active). Run `envelope license activate` to add more."
                );
            }
            std::process::exit(1);
        }
    }

    let created = db
        .create_agent(name)
        .with_context(|| format!("failed to create agent '{name}'"))?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "status": "created",
                "id": created.identity.id,
                "name": created.identity.name,
                "token": created.token,
                "token_prefix": created.identity.token_prefix,
            }))?
        );
    } else {
        println!("Created agent '{}'", created.identity.name);
        println!("  id:     {}", created.identity.id);
        println!("  token:  {}", created.token);
        println!();
        println!("Store this token now — it is shown ONCE and cannot be recovered.");
        println!("Set ENVELOPE_AGENT_TOKEN=<token> in the agent's MCP env to enforce its policy.");
    }
    Ok(())
}

// ── list ────────────────────────────────────────────────────────────

pub fn run_list(json: bool, _backend: CredentialBackend) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let agents = db.list_agents().context("failed to list agents")?;

    if json {
        let rows: Vec<_> = agents.iter().map(identity_json).collect();
        println!("{}", serde_json::to_string_pretty(&json!(rows))?);
        return Ok(());
    }

    if agents.is_empty() {
        println!("No agents found");
        return Ok(());
    }
    println!(
        "{:<20}  {:<16}  {:<10}  LAST USED",
        "NAME", "TOKEN PREFIX", "STATUS"
    );
    println!("{}", "-".repeat(72));
    for a in &agents {
        let status = if a.revoked_at.is_some() {
            "revoked"
        } else {
            "active"
        };
        println!(
            "{:<20}  {:<16}  {:<10}  {}",
            a.name,
            a.token_prefix,
            status,
            a.last_used_at.as_deref().unwrap_or("-")
        );
    }
    println!("\n{} agent(s)", agents.len());
    Ok(())
}

// ── show ────────────────────────────────────────────────────────────

pub fn run_show(name: &str, json: bool, _backend: CredentialBackend) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let agent = db
        .get_agent_by_name(name)
        .context("failed to load agent")?
        .ok_or_else(|| anyhow::anyhow!("agent not found: {name}"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&identity_json(&agent))?);
    } else {
        println!("Agent '{}'", agent.name);
        println!("  id:           {}", agent.id);
        println!("  token prefix: {}", agent.token_prefix);
        println!("  created:      {}", agent.created_at);
        println!(
            "  revoked:      {}",
            agent.revoked_at.as_deref().unwrap_or("-")
        );
        println!(
            "  last used:    {}",
            agent.last_used_at.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

// ── revoke ──────────────────────────────────────────────────────────

pub fn run_revoke(name: &str, json: bool, _backend: CredentialBackend) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let agent = db
        .get_agent_by_name(name)
        .context("failed to load agent")?
        .ok_or_else(|| anyhow::anyhow!("agent not found: {name}"))?;
    db.revoke_agent(&agent.id)
        .context("failed to revoke agent")?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "status": "revoked",
                "id": agent.id,
                "name": agent.name,
            }))?
        );
    } else {
        println!("Revoked agent '{}'", agent.name);
    }
    Ok(())
}

// ── policy set / show ───────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn run_policy_set(
    name: &str,
    allow_accounts: Option<&str>,
    allow_folders: Option<&str>,
    allow_actions: Option<&str>,
    send_mode_ceiling: Option<&str>,
    allow_recipients: Option<&str>,
    json: bool,
    _backend: CredentialBackend,
) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let agent = db
        .get_agent_by_name(name)
        .context("failed to load agent")?
        .ok_or_else(|| anyhow::anyhow!("agent not found: {name}"))?;

    // Start from the agent's existing policy, or the safe default.
    let mut policy = db
        .get_agent_policy(&agent.id)
        .context("failed to read policy")?
        .unwrap_or_else(|| StoreAgentPolicy::default_for(&agent.id));

    if let Some(v) = allow_accounts {
        policy.allowed_accounts = encode_allow_list(v)?;
    }
    if let Some(v) = allow_folders {
        policy.allowed_folders = encode_allow_list(v)?;
    }
    if let Some(v) = allow_actions {
        policy.allowed_actions = encode_allow_list(v)?;
    }
    if let Some(v) = allow_recipients {
        policy.allow_recipients = Some(encode_allow_list(v)?);
    }
    if let Some(v) = send_mode_ceiling {
        // Validate against the four stable names.
        policy.send_mode_ceiling = SendModeCeiling::parse(v)
            .map_err(|_| anyhow::anyhow!("invalid --send-mode-ceiling: {v} (expected one of draft-only, confirm-send, allowlisted-send, autonomous-send)"))?;
    }

    db.set_agent_policy(&policy)
        .context("failed to store policy")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&policy_json(&policy))?);
    } else {
        println!("Updated policy for agent '{}'", agent.name);
        print_policy(&policy);
    }
    Ok(())
}

pub fn run_policy_show(name: &str, json: bool, _backend: CredentialBackend) -> Result<()> {
    let db = Database::open_default().context("failed to open database")?;
    let agent = db
        .get_agent_by_name(name)
        .context("failed to load agent")?
        .ok_or_else(|| anyhow::anyhow!("agent not found: {name}"))?;
    let policy = db
        .get_agent_policy(&agent.id)
        .context("failed to read policy")?
        .unwrap_or_else(|| StoreAgentPolicy::default_for(&agent.id));

    if json {
        println!("{}", serde_json::to_string_pretty(&policy_json(&policy))?);
    } else {
        println!("Policy for agent '{}'", agent.name);
        print_policy(&policy);
    }
    Ok(())
}

// ── helpers ─────────────────────────────────────────────────────────

fn identity_json(a: &envelope_email_store::AgentIdentity) -> serde_json::Value {
    json!({
        "id": a.id,
        "name": a.name,
        "token_prefix": a.token_prefix,
        "created_at": a.created_at,
        "revoked_at": a.revoked_at,
        "last_used_at": a.last_used_at,
        "status": if a.revoked_at.is_some() { "revoked" } else { "active" },
    })
}

fn policy_json(p: &StoreAgentPolicy) -> serde_json::Value {
    json!({
        "agent_id": p.agent_id,
        "allowed_accounts": decode_for_display(&p.allowed_accounts),
        "allowed_folders": decode_for_display(&p.allowed_folders),
        "allowed_actions": decode_for_display(&p.allowed_actions),
        "send_mode_ceiling": p.send_mode_ceiling.as_str(),
        "allow_recipients": p.allow_recipients.as_deref().map(decode_for_display),
    })
}

fn print_policy(p: &StoreAgentPolicy) {
    println!("  accounts:  {}", p.allowed_accounts);
    println!("  folders:   {}", p.allowed_folders);
    println!("  actions:   {}", p.allowed_actions);
    println!("  ceiling:   {}", p.send_mode_ceiling.as_str());
    println!(
        "  recipients:{}",
        p.allow_recipients.as_deref().unwrap_or(" -")
    );
}

/// Turn a CLI `--allow-*` value into the stored allow-list encoding: the literal
/// `"*"` stays a wildcard; a comma-separated list becomes a JSON string array.
/// Empty entries are dropped; an all-empty input is deny-all (`[]`).
fn encode_allow_list(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed == "*" {
        return Ok("*".to_string());
    }
    let entries: Vec<String> = trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if entries.iter().any(|e| e == "*") {
        // Mixing "*" with explicit entries is ambiguous; refuse it.
        bail!("allow-list may be \"*\" (allow all) or a comma-separated list, not both");
    }
    Ok(serde_json::to_string(&entries)?)
}

/// Decode a stored allow-list back into a JSON-friendly display value: `"*"`
/// stays a string, a JSON array becomes an array, anything else is echoed.
fn decode_for_display(raw: &str) -> serde_json::Value {
    if raw.trim() == "*" {
        return json!("*");
    }
    serde_json::from_str::<serde_json::Value>(raw).unwrap_or_else(|_| json!(raw))
}
