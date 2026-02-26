# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import json
from datetime import datetime, timezone

from mcp.server.fastmcp import FastMCP

from app.services.policy import get_domain_policy, list_address_policies
from app.services.actions import log_action
from app.services import start_here as start_here_svc
from app import drafts as drafts_module

mcp = FastMCP(
    "Envelope Email",
    instructions=(
        "Envelope is a programmable email API. "
        "Call start_here first with the account_id to get the domain policy and "
        "address policies that govern how this account should behave. "
        "Always log every action you take using log_action."
    ),
)


@mcp.tool()
async def start_here(account_id: str) -> str:
    """Get the account's configuration, policies, and agent instructions.

    ALWAYS call this tool first before doing anything with an email account.
    Returns onboarding instructions if the account is not yet configured,
    or operational instructions + policies if it is.

    Do NOT call this repeatedly in a single session — call once and cache the result.
    """
    result = await start_here_svc.build_start_here_response(account_id)
    return json.dumps(result, indent=2)


@mcp.tool()
async def get_domain_policy_tool(account_id: str) -> str:
    """Get the full domain-level policy including kb_text for an account.

    Use this when start_here returns kb_text_truncated=true to fetch the complete
    knowledge base text. Otherwise start_here already includes the policy summary.
    """
    policy = await get_domain_policy(account_id)
    if not policy:
        return json.dumps({"error": "No domain policy found", "account_id": account_id})
    return json.dumps(policy, indent=2)


@mcp.tool()
async def list_address_policies_tool(account_id: str) -> str:
    """List all address policies for an account.

    Returns patterns and their handling rules. Use to determine how to process
    messages from specific senders. Match sender addresses against patterns using
    fnmatch (exact match first, then wildcard).

    Do NOT call this if start_here already returned address_policies — use the
    cached data from start_here instead.
    """
    policies = await list_address_policies(account_id)
    return json.dumps(policies, indent=2)


@mcp.tool()
async def log_action_tool(
    account_id: str,
    action_type: str,
    confidence: float,
    justification: str,
    action_taken: str,
    message_id: str = None,
    draft_id: str = None,
) -> str:
    """Log an action taken by the agent. Required after every decision.

    action_type must be one of: inbound_route, draft_approve, draft_reject,
    send_decision, escalate, trash

    confidence: 0.0-1.0, how confident you are in this action
    justification: explain WHY you took this action, citing the policy
    action_taken: describe WHAT you actually did

    Always log before and after significant actions, not just at the end.
    """
    entry = await log_action(
        account_id=account_id,
        action_type=action_type,
        confidence=confidence,
        justification=justification,
        action_taken=action_taken,
        message_id=message_id,
        draft_id=draft_id,
    )
    return json.dumps(entry, indent=2)


@mcp.tool()
async def create_draft_tool(
    account_id: str,
    to: str,
    subject: str = None,
    text: str = None,
    html: str = None,
    in_reply_to: str = None,
    created_by: str = "agent",
) -> str:
    """Create a draft email for human review.

    Use this when confidence is below the account's confidence_threshold,
    or when the policy requires human approval before sending.

    Set in_reply_to to the Message-ID of the email being replied to.
    created_by should be 'agent' for agent-created drafts.
    """
    draft = await drafts_module.create_draft(
        account_id=account_id,
        to_addr=to,
        subject=subject,
        text_content=text,
        html_content=html,
        in_reply_to=in_reply_to,
        created_by=created_by,
    )
    return json.dumps(draft, indent=2)


@mcp.tool()
async def get_draft_tool(account_id: str, draft_id: str) -> str:
    """Get a draft by ID.

    Use to check the current state of a draft before approving or rejecting it.
    """
    draft = await drafts_module.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        return json.dumps({"error": "Draft not found"})
    return json.dumps(draft, indent=2)


@mcp.tool()
async def send_email_tool(
    account_id: str,
    to: str,
    subject: str,
    text: str = None,
    html: str = None,
) -> str:
    """Send an email immediately without creating a draft.

    Use this ONLY when confidence is high and policy permits direct send.
    For low-confidence actions, use create_draft_tool instead.

    Requires the account to have valid SMTP credentials configured.
    """
    from app.credentials.store import get_account_with_credentials
    from app.transport.smtp import build_mime_message, send_message, SmtpSendError
    from app import messages

    account = await get_account_with_credentials(account_id)
    if not account:
        return json.dumps({"error": "Account not found"})

    from_addr = account["username"]
    msg = build_mime_message(
        from_addr=from_addr,
        to_addr=to,
        subject=subject,
        text=text,
        html=html,
        display_name=account.get("display_name"),
    )

    record = await messages.create_message(
        account_id=account_id,
        from_addr=from_addr,
        to_addr=to,
        subject=subject,
        text_content=text,
        html_content=html,
    )

    try:
        smtp_message_id = await send_message(account, msg, pool=None)
    except SmtpSendError as e:
        await messages.mark_failed(record["id"], e.message)
        return json.dumps({"error": e.message, "error_type": e.error_type})

    await messages.mark_sent(record["id"], smtp_message_id)
    return json.dumps({"status": "sent", "id": record["id"], "message_id": smtp_message_id})


@mcp.tool()
async def approve_draft_tool(account_id: str, draft_id: str) -> str:
    """Approve and send a draft immediately.

    Use this when a draft has been reviewed and is ready to send.
    Only works on drafts with status='draft'. Records approval metadata.

    Do NOT use this for drafts awaiting human review in the review queue —
    those are for humans to approve via the /review interface.
    """
    from app.credentials.store import get_account_with_credentials
    from app.transport.smtp import build_mime_message, send_message, SmtpSendError
    from app import messages

    draft = await drafts_module.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        return json.dumps({"error": "Draft not found"})
    if draft["status"] != "draft":
        return json.dumps({"error": f"Cannot send draft with status '{draft['status']}'"})

    account = await get_account_with_credentials(account_id)
    if not account:
        return json.dumps({"error": "Account not found"})

    # Record approval metadata
    existing_meta = draft.get("metadata") or {}
    existing_meta["approved_at"] = datetime.now(timezone.utc).isoformat()
    existing_meta["approved_by"] = "agent"
    await drafts_module.update_draft(draft_id, metadata=existing_meta)

    from_addr = account["username"]
    msg = build_mime_message(
        from_addr=from_addr,
        to_addr=draft["to_addr"],
        subject=draft["subject"] or "",
        text=draft["text_content"],
        html=draft["html_content"],
        display_name=account.get("display_name"),
    )
    if draft["in_reply_to"]:
        msg["In-Reply-To"] = draft["in_reply_to"]

    record = await messages.create_message(
        account_id=account_id,
        from_addr=from_addr,
        to_addr=draft["to_addr"],
        subject=draft["subject"],
        text_content=draft["text_content"],
        html_content=draft["html_content"],
    )

    try:
        smtp_message_id = await send_message(account, msg, pool=None)
    except SmtpSendError as e:
        await messages.mark_failed(record["id"], e.message)
        return json.dumps({"error": e.message, "error_type": e.error_type})

    await messages.mark_sent(record["id"], smtp_message_id)
    await drafts_module.mark_draft_sent(draft_id, record["id"])
    return json.dumps({"status": "sent", "draft_id": draft_id, "message_id": record["id"]})


@mcp.tool()
async def reject_draft_tool(
    account_id: str,
    draft_id: str,
    feedback: str = None,
) -> str:
    """Reject and discard a draft.

    Use when a draft does not meet policy requirements and should not be sent.
    Provide feedback explaining why the draft was rejected.

    Do NOT reject drafts that are pending human review in the review queue —
    those are for humans to decide.
    """
    draft = await drafts_module.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        return json.dumps({"error": "Draft not found"})
    if draft["status"] != "draft":
        return json.dumps({"error": f"Cannot reject draft with status '{draft['status']}'"})

    existing_meta = draft.get("metadata") or {}
    existing_meta["rejected_at"] = datetime.now(timezone.utc).isoformat()
    if feedback:
        existing_meta["rejection_feedback"] = feedback
    await drafts_module.update_draft(draft_id, metadata=existing_meta)
    await drafts_module.discard_draft(draft_id)
    return json.dumps({"status": "rejected", "id": draft_id})
