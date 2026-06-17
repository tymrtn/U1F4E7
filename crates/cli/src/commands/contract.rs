// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Versioned agent-facing JSON contract for Envelope CLI and MCP surfaces.
//!
//! Existing command JSON output remains unchanged. Breaking contract changes
//! should create a new `envelope.agent_contract.vN` schema.

use anyhow::Result;
use serde_json::{Value, json};

pub const AGENT_CONTRACT_SCHEMA: &str = "envelope.agent_contract.v1";

/// Default summary count returned by read-only agent list/search surfaces.
pub const DEFAULT_AGENT_LIST_LIMIT: u32 = 25;

/// Maximum summary count an agent/CLI caller may request from read-only
/// list/search surfaces. Dashboard endpoints intentionally use their own
/// lower caps and are not affected by this constant.
pub const MAX_AGENT_LIST_LIMIT: u32 = 1000;

pub fn run(surface_name: Option<&str>) -> Result<()> {
    let output = match surface_name {
        Some(name) => {
            surface(name).ok_or_else(|| anyhow::anyhow!("unknown contract surface: {name}"))?
        }
        None => agent_contract(),
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

pub fn agent_contract() -> Value {
    json!({
        "schema": AGENT_CONTRACT_SCHEMA,
        "compatibility": {
            "breaking_change_policy": "Field removals, required-field additions, type changes, and semantic renames require a new schema id. New optional fields are backward-compatible.",
            "output_contract": "Existing CLI --json output is not changed by this contract export.",
            "secrets_policy": "Contracts, examples, tests, logs, and errors must not include passwords, OAuth tokens, app passwords, or raw OTP values unless the command purpose is OTP retrieval."
        },
        "consumers": ["cli", "mcp", "hermes", "codex"],
        "surfaces": surfaces(),
        "mcp_tools": mcp_tool_entries(),
    })
}

pub fn surface(name: &str) -> Option<Value> {
    surfaces()
        .as_array()
        .expect("static surfaces array")
        .iter()
        .find(|surface| surface["name"] == name)
        .cloned()
}

pub fn mcp_tool_list() -> Value {
    json!({ "tools": mcp_tool_entries() })
}

fn surfaces() -> Value {
    let mut items = Vec::new();

    items.push(surface_entry(
        "inbox",
        "envelope inbox --json",
        Some("inbox"),
        object(
            json!({
                "folder": string_default("IMAP folder name", "INBOX"),
                "limit": integer_default_range(
                    "Maximum messages to return",
                    DEFAULT_AGENT_LIST_LIMIT as u64,
                    1,
                    MAX_AGENT_LIST_LIMIT as u64,
                ),
                "account": string("Account ID or email address; default account if omitted")
            }),
            json!([]),
        ),
        array_of(message_summary_schema()),
        vec![
            "Message summary fields mirror transport EmailSummary serialization.",
            "Agent/CLI limit is capped at 1000; dashboard endpoints keep their own lower caps.",
        ],
    ));
    items.push(surface_entry(
        "read",
        "envelope read <uid> --json",
        Some("read"),
        object(
            json!({
                "uid": integer("Message UID"),
                "folder": string_default("IMAP folder", "INBOX"),
                "account": string("Account ID or email address")
            }),
            json!(["uid"]),
        ),
        message_detail_schema(),
        vec!["Read uses non-mutating fetch behavior and must not mark messages read."],
    ));
    items.push(surface_entry(
        "search",
        "envelope search <query> --json",
        Some("search"),
        object(
            json!({
                "query": string("IMAP search query"),
                "folder": string_default("IMAP folder", "INBOX"),
                "limit": integer_default_range(
                    "Maximum results",
                    DEFAULT_AGENT_LIST_LIMIT as u64,
                    1,
                    MAX_AGENT_LIST_LIMIT as u64,
                ),
                "account": string("Account ID or email address"),
                "roles": json!({
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Folder roles to search instead of --folder: inbox, drafts, sent, trash, spam, archive, starred. Resolves provider-specific layouts (e.g. INBOX/sent, [Gmail]/Sent Mail); results include the source folder. Read-only."
                })
            }),
            json!(["query"]),
        ),
        array_of(message_summary_schema()),
        vec![
            "Search syntax is passed through to the IMAP server.",
            "Agent/CLI limit is capped at 1000; dashboard endpoints keep their own lower caps.",
            "--role/--roles searches every folder matching the role and errors if a role resolves to zero folders.",
        ],
    ));
    items.push(surface_entry(
        "send",
        "envelope send --to --subject --json",
        Some("send"),
        send_input_schema(),
        object(
            json!({
                "status": string("sent, scheduled, drafted, or denied"),
                "sent": json!({"type": "boolean", "description": "MCP send/reply result flag when available"}),
                "send_mode": string("Applied send safety mode when policy was evaluated"),
                "error": json!({"type": "object", "description": "Stable denial object for denied sends"}),
                "message_id": string("SMTP Message-ID when sent immediately"),
                "sent_folder": string("Sent folder containing the sent message when resolved"),
                "sent_uid": json!({"type": ["integer", "null"], "description": "Sent-folder IMAP UID when resolved"}),
                "sent_message_url": string("Dashboard URL for the sent message when resolved"),
                "sent_mail": json!({"type": "object", "description": "Sent mailbox proof: folder, uid, message_url, lookup_status, lookup_error, and ui"}),
                "draft_id": string("Local draft id when scheduled or draft-only"),
                "to": string("Recipient address"),
                "subject": string("Subject")
            }),
            json!([]),
        ),
        vec!["No output fields contain SMTP credentials or attachment bytes."],
    ));
    items.push(surface_entry(
        "thread",
        "envelope thread show/list/build --json",
        None,
        object(
            json!({
                "uid": integer("Message UID for thread show/build source"),
                "folder": string_default("IMAP folder", "INBOX"),
                "limit": integer_default("Maximum threads/messages", 50),
                "account": string("Account ID or email address")
            }),
            json!([]),
        ),
        object(
            json!({
                "thread_id": string("Stable local thread identifier"),
                "subject": string("Normalized thread subject"),
                "message_count": integer("Message count"),
                "messages": array_of(json!({"type": "object"}))
            }),
            json!([]),
        ),
        vec!["Evidence thread expansion remains header-based and bounded."],
    ));
    items.push(surface_entry(
        "draft",
        "envelope draft create/list/send/discard --json",
        None,
        object(
            json!({
                "to": string("Recipient email address"),
                "subject": string("Draft subject"),
                "body": string("Plain-text draft body"),
                "cc": string("CC recipients"),
                "bcc": string("BCC recipients"),
                "in_reply_to": string("Optional message UID or Message-ID to reply to"),
                "account": string("Account ID or email address")
            }),
            json!([]),
        ),
        object(
            json!({
                "draft_id": string("Local draft id"),
                "status": string("created, sent, discarded, or stored status"),
                "imap_uid": json!({"type": ["integer", "null"], "description": "IMAP Drafts UID when present"}),
                "message_id": string("SMTP Message-ID for sent drafts"),
                "sent_folder": string("Sent folder containing the sent message when resolved"),
                "sent_uid": json!({"type": ["integer", "null"], "description": "Sent-folder IMAP UID when resolved"}),
                "sent_message_url": string("Dashboard URL for the sent message when resolved"),
                "sent_mail": json!({"type": "object", "description": "Sent mailbox proof: folder, uid, message_url, lookup_status, lookup_error, and ui"})
            }),
            json!([]),
        ),
        vec!["Agent send flows should draft first, then send only after explicit human approval."],
    ));
    items.push(surface_entry(
        "watch",
        "envelope watch --json",
        None,
        object(
            json!({
                "folder": string_default("IMAP folder to watch", "INBOX"),
                "account": string("Account ID or email address"),
                "webhook": string("Optional URL receiving the same JSON event"),
                "run_rules": json!({"type": "boolean", "description": "Run rules against new messages when implemented", "default": false})
            }),
            json!([]),
        ),
        object(
            json!({
                "event_id": string("Local event id"),
                "event_type": string("new_message or otp_detected"),
                "idempotency_key": string("Stable event dedupe key"),
                "account_id": string("Account id"),
                "folder": string("Folder"),
                "uid": integer("Message UID"),
                "secure_payload": json!({"type": "object", "description": "Redacted structured payload; OTP values are not emitted in watch events"})
            }),
            json!([]),
        ),
        vec!["Watch emits newline-delimited JSON events; consumers must parse per line."],
    ));
    items.push(surface_entry(
        "otp",
        "envelope code --json",
        None,
        object(
            json!({
                "account": string("Account ID or email address"),
                "from": string("Sender address/domain substring filter"),
                "subject": string("Subject substring filter"),
                "wait": integer_default("Seconds to wait before timeout", 120)
            }),
            json!([]),
        ),
        object(
            json!({
                "code": string("Verification code returned only by explicit OTP command"),
                "source_uid": integer("Message UID containing code"),
                "confidence": json!({"type": "number", "description": "Extractor confidence 0.0-1.0"}),
                "source_pattern": string("Extractor pattern id")
            }),
            json!([]),
        ),
        vec!["Watch/event payloads redact OTP value; envelope code may return it."],
    ));
    items.push(surface_entry(
        "rules",
        "envelope rule create/list/test/run/export --json",
        None,
        object(
            json!({
                "name": string("Rule name"),
                "match_from": string("From substring predicate"),
                "match_to": string("To substring predicate"),
                "match_subject": string("Subject substring predicate"),
                "match_tag": array_of(json!({"type": "string"})),
                "action": string("Rule action expression"),
                "priority": integer_default("Rule priority", 100),
                "stop": json!({"type": "boolean", "description": "Stop after match", "default": false}),
                "account": string("Account ID or email address")
            }),
            json!([]),
        ),
        object(
            json!({
                "rule_id": string("Rule id"),
                "rule_name": string("Rule name"),
                "matches": array_of(json!({"type": "object"})),
                "processed": integer("Messages processed by run"),
                "actions": integer("Actions taken by run"),
                "log": array_of(json!({"type": "object"}))
            }),
            json!([]),
        ),
        vec!["Webhook actions must redact secrets in display, logs, docs, and tests."],
    ));
    items.push(surface_entry(
        "evidence",
        "envelope evidence collect/verify/attachment export --json",
        None,
        object(
            json!({
                "account": string("Account ID or email address"),
                "folder": string_default("IMAP folder", "INBOX"),
                "query": string("IMAP search query"),
                "include_thread": json!({"type": "boolean", "description": "Include bounded header-linked thread expansion", "default": false}),
                "max_thread_messages": integer_default("Maximum messages in thread expansion", 500),
                "out": string("Output bundle or attachment-export directory"),
                "uid": integer("Single source UID for attachment export (mutually exclusive with query)"),
                "attachment": string("Exact original attachment filename for attachment export"),
                "filename_glob": string("Case-insensitive attachment filename glob for attachment export"),
                "extract_text": json!({"type": "boolean", "description": "Extract DOCX/text attachment text during attachment export", "default": false})
            }),
            json!(["out"]),
        ),
        object(
            json!({
                "schema": string("Evidence manifest schema id"),
                "status": string("collected or verified"),
                "manifest_path": string("Manifest path"),
                "message_count": integer("Canonical .eml count"),
                "checksums": json!({"type": "object", "description": "Manifest/index/hash material"}),
                "warnings": array_of(json!({"type": "string"}))
            }),
            json!([]),
        ),
        vec!["Collection and attachment export must use EXAMINE and BODY.PEEK[]; raw RFC822 .eml files and raw attachment bytes remain canonical evidence."],
    ));

    for (name, schema) in mcp_only_inputs() {
        items.push(surface_entry(
            name,
            "mcp-only",
            Some(name),
            schema,
            json!({"type": "object"}),
            vec![],
        ));
    }

    Value::Array(items)
}

fn mcp_tool_entries() -> Value {
    let descriptions = [
        (
            "inbox",
            "List messages in a mailbox folder. Returns message summaries with UID, from, subject, date, and flags.",
        ),
        (
            "read",
            "Read a full email message by UID. Returns headers, text body, HTML body, and attachment metadata. Does not mark the message as read.",
        ),
        (
            "search",
            "Search messages using IMAP search syntax. Examples: 'FROM boss@company.com', 'SUBJECT invoice', 'UNSEEN'.",
        ),
        (
            "send",
            "Send an email. Supports text and HTML bodies, CC, BCC, reply-to, and file attachments.",
        ),
        (
            "reply",
            "Reply to a message. Automatically sets In-Reply-To, References, and subject prefix.",
        ),
        (
            "create_reply_draft",
            "Create a Mail.app-style contextual reply draft with populated threading headers, preserved quoted context, and abridged preview.",
        ),
        (
            "create_forward_draft",
            "Create a Mail.app-style contextual forward draft with forwarded-message context and abridged preview.",
        ),
        (
            "modify_draft",
            "Modify the agent-authored portion of a contextual draft while preserving quote/forward context and threading metadata.",
        ),
        (
            "get_draft",
            "Fetch a stored draft envelope with metadata and abridged contextual preview.",
        ),
        (
            "send_draft",
            "Send a draft by draft id. Requires explicit confirmation in agent contexts.",
        ),
        ("move_message", "Move a message to another IMAP folder."),
        (
            "flag",
            "Add or remove IMAP flags on a message. Common flags: \\Seen, \\Flagged, \\Answered, \\Draft, \\Deleted.",
        ),
        (
            "folders",
            "List IMAP folders with message counts (exists/unseen).",
        ),
        (
            "tag",
            "Set tags and scores on a message. Tags are freeform strings, scores are named dimensions with float values (0.0-1.0). Used by the rules engine.",
        ),
        (
            "contacts",
            "Manage contacts. Supports list, add, show, and tag operations.",
        ),
        ("accounts", "List configured email accounts."),
    ];

    Value::Array(
        descriptions
            .iter()
            .map(|(name, description)| {
                let surface =
                    surface(name).unwrap_or_else(|| panic!("missing MCP contract surface: {name}"));
                let mut input_schema = surface["input_schema"].clone();
                if *name == "send" {
                    if let Some(send_mode) = input_schema
                        .get_mut("properties")
                        .and_then(|props| props.get_mut("send_mode"))
                    {
                        send_mode["default"] = json!("draft-only");
                        send_mode["description"] = json!(
                            "MCP send safety mode; defaults to draft-only for agent contexts"
                        );
                    }
                }
                json!({
                    "name": name,
                    "description": description,
                    "inputSchema": input_schema,
                    "contractSchema": AGENT_CONTRACT_SCHEMA,
                })
            })
            .collect(),
    )
}

fn mcp_only_inputs() -> Vec<(&'static str, Value)> {
    vec![
        (
            "reply",
            object(
                json!({
                    "uid": integer("UID of message to reply to"),
                    "body": string("Reply text body"),
                    "html": string("Reply HTML body"),
                    "reply_all": json!({"type": "boolean", "description": "Reply to all recipients", "default": false}),
                    "send_mode": json!({"type": "string", "enum": ["draft-only", "confirm-send", "allowlisted-send", "autonomous-send"], "default": "draft-only", "description": "MCP reply safety mode"}),
                    "confirm_send": json!({"type": "boolean", "default": false, "description": "Required when send_mode is confirm-send"}),
                    "allow_recipient": array_of(json!({"type": "string", "description": "Allowed recipient email or domain for allowlisted-send"})),
                    "folder": string_default("IMAP folder of original message", "INBOX"),
                    "account": string("Account ID or email address")
                }),
                json!(["uid", "body"]),
            ),
        ),
        (
            "create_reply_draft",
            object(
                json!({
                    "uid": integer("UID of message to reply to"),
                    "folder": string_default("IMAP folder of original message", "INBOX"),
                    "reply_all": json!({"type": "boolean", "description": "Reply to all recipients", "default": false}),
                    "body": string("Initial agent-authored plain-text body"),
                    "html": string("Initial agent-authored HTML body"),
                    "add_signature": json!({"type": "boolean", "description": "Append the account signature when available", "default": false}),
                    "account": string("Account ID or email address")
                }),
                json!(["uid"]),
            ),
        ),
        (
            "create_forward_draft",
            object(
                json!({
                    "uid": integer("UID of message to forward"),
                    "folder": string_default("IMAP folder of source message", "INBOX"),
                    "to": string("Optional forward recipient; may be left empty for later edit"),
                    "body": string("Initial agent-authored plain-text body"),
                    "html": string("Initial agent-authored HTML body"),
                    "add_signature": json!({"type": "boolean", "description": "Append the account signature when available", "default": false}),
                    "account": string("Account ID or email address")
                }),
                json!(["uid"]),
            ),
        ),
        (
            "modify_draft",
            object(
                json!({
                    "draft_id": string("Local draft id"),
                    "body": string("Replacement agent-authored plain-text body"),
                    "html": string("Replacement agent-authored HTML body"),
                    "to": string("Recipient override"),
                    "cc": string("CC override"),
                    "bcc": string("BCC override"),
                    "subject": string("Subject override"),
                    "add_signature": json!({"type": "boolean", "description": "Override signature application for this edit"}),
                    "account": string("Account ID or email address")
                }),
                json!(["draft_id"]),
            ),
        ),
        (
            "get_draft",
            object(
                json!({
                    "draft_id": string("Local draft id")
                }),
                json!(["draft_id"]),
            ),
        ),
        (
            "send_draft",
            object(
                json!({
                    "draft_id": string("Local draft id"),
                    "confirm_send": json!({"type": "boolean", "description": "Required to send a draft from MCP", "default": false}),
                    "account": string("Account ID or email address")
                }),
                json!(["draft_id"]),
            ),
        ),
        (
            "move_message",
            object(
                json!({
                    "uid": integer("Message UID"),
                    "to_folder": string("Destination folder"),
                    "from_folder": string_default("Source folder", "INBOX"),
                    "account": string("Account ID or email address")
                }),
                json!(["uid", "to_folder"]),
            ),
        ),
        (
            "flag",
            object(
                json!({
                    "uid": integer("Message UID"),
                    "action": json!({"type": "string", "enum": ["add", "remove"], "description": "Add or remove the flag"}),
                    "flag": string("IMAP flag name"),
                    "folder": string_default("IMAP folder", "INBOX"),
                    "account": string("Account ID or email address")
                }),
                json!(["uid", "action", "flag"]),
            ),
        ),
        (
            "folders",
            object(
                json!({"account": string("Account ID or email address")}),
                json!([]),
            ),
        ),
        (
            "tag",
            object(
                json!({
                    "uid": integer("Message UID"),
                    "tags": array_of(json!({"type": "string"})),
                    "scores": json!({"type": "object", "additionalProperties": {"type": "number"}, "description": "Score dimensions"}),
                    "folder": string_default("IMAP folder", "INBOX"),
                    "account": string("Account ID or email address")
                }),
                json!(["uid"]),
            ),
        ),
        (
            "contacts",
            object(
                json!({
                    "action": json!({"type": "string", "enum": ["list", "add", "show", "tag", "untag"], "description": "Contact operation"}),
                    "email": string("Contact email address"),
                    "name": string("Contact name"),
                    "tag": string("Contact tag"),
                    "notes": string("Contact notes"),
                    "account": string("Account ID or email address")
                }),
                json!(["action"]),
            ),
        ),
        ("accounts", object(json!({}), json!([]))),
    ]
}

fn surface_entry(
    name: &str,
    cli_command: &str,
    mcp_tool: Option<&str>,
    input_schema: Value,
    output_schema: Value,
    compatibility_notes: Vec<&str>,
) -> Value {
    json!({
        "name": name,
        "stability": "stable-v1",
        "cli": { "command": cli_command, "json_output": true },
        "mcp": { "tool": mcp_tool, "implemented": mcp_tool.is_some() },
        "input_schema": input_schema,
        "output_schema": output_schema,
        "compatibility_notes": compatibility_notes,
    })
}

fn send_input_schema() -> Value {
    object(
        json!({
            "to": string("Recipient email address"),
            "subject": string("Email subject"),
            "body": string("Plain text body"),
            "html": string("HTML body sent alongside text"),
            "cc": string("CC recipients"),
            "bcc": string("BCC recipients"),
            "reply_to": string("Reply-To address"),
            "from": string("Override sender identity"),
            "send_mode": json!({"type": "string", "enum": ["draft-only", "confirm-send", "allowlisted-send", "autonomous-send"], "default": "autonomous-send", "description": "CLI send safety mode; MCP defaults this field to draft-only"}),
            "confirm_send": json!({"type": "boolean", "default": false, "description": "Required when send_mode is confirm-send"}),
            "allow_recipient": array_of(string("Allowed email address or domain for allowlisted-send")),
            "account": string("Account ID or email address")
        }),
        json!(["to", "subject"]),
    )
}

fn object(properties: Value, required: Value) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn string_default(description: &str, default: &str) -> Value {
    json!({ "type": "string", "description": description, "default": default })
}

fn integer(description: &str) -> Value {
    json!({ "type": "integer", "description": description })
}

fn integer_default(description: &str, default: u64) -> Value {
    json!({ "type": "integer", "description": description, "default": default })
}

fn integer_default_range(description: &str, default: u64, minimum: u64, maximum: u64) -> Value {
    json!({
        "type": "integer",
        "description": description,
        "default": default,
        "minimum": minimum,
        "maximum": maximum,
    })
}

fn array_of(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

fn message_summary_schema() -> Value {
    object(
        json!({
            "uid": integer("Message UID"),
            "from_addr": string("Sender address"),
            "subject": string("Subject"),
            "date": string("Message date"),
            "flags": array_of(json!({"type": "string"})),
            "message_id": string("Message-ID header when available")
        }),
        json!([]),
    )
}

fn message_detail_schema() -> Value {
    object(
        json!({
            "uid": integer("Message UID"),
            "from_addr": string("Sender address"),
            "to_addr": string("First recipient address (compat; see to_addrs for full list)"),
            "cc_addr": string("First Cc address (compat; see cc_addrs for full list)"),
            "to_addrs": array_of(json!({"type": "string"})),
            "cc_addrs": array_of(json!({"type": "string"})),
            "subject": string("Subject"),
            "date": string("Message date"),
            "text_body": string("Plain-text body"),
            "html_body": string("HTML body"),
            "attachments": array_of(json!({"type": "object"})),
            "message_id": string("Message-ID header when available"),
            "in_reply_to": string("In-Reply-To header when available"),
            "references": string("References header when available")
        }),
        json!([]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit_schema_for(surface_name: &str) -> Value {
        let s = surface(surface_name).expect("contract surface");
        s["input_schema"]["properties"]["limit"].clone()
    }

    #[test]
    fn inbox_surface_limit_advertises_agent_max_and_min() {
        let limit = limit_schema_for("inbox");
        assert_eq!(limit["default"], json!(25));
        assert_eq!(limit["maximum"], json!(1000));
        assert_eq!(limit["minimum"], json!(1));
    }

    #[test]
    fn search_surface_limit_advertises_agent_max_and_min() {
        let limit = limit_schema_for("search");
        assert_eq!(limit["default"], json!(25));
        assert_eq!(limit["maximum"], json!(1000));
        assert_eq!(limit["minimum"], json!(1));
    }

    #[test]
    fn mcp_tool_inbox_limit_advertises_agent_max_and_min() {
        let tools = mcp_tool_list();
        let entries = tools["tools"].as_array().expect("mcp tools array");
        let inbox = entries
            .iter()
            .find(|t| t["name"] == "inbox")
            .expect("inbox tool");
        let limit = &inbox["inputSchema"]["properties"]["limit"];
        assert_eq!(limit["default"], json!(25));
        assert_eq!(limit["maximum"], json!(1000));
        assert_eq!(limit["minimum"], json!(1));
    }

    #[test]
    fn mcp_tool_search_limit_advertises_agent_max_and_min() {
        let tools = mcp_tool_list();
        let entries = tools["tools"].as_array().expect("mcp tools array");
        let search = entries
            .iter()
            .find(|t| t["name"] == "search")
            .expect("search tool");
        let limit = &search["inputSchema"]["properties"]["limit"];
        assert_eq!(limit["default"], json!(25));
        assert_eq!(limit["maximum"], json!(1000));
        assert_eq!(limit["minimum"], json!(1));
    }
}
