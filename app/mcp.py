# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import json
from datetime import datetime, timezone
from typing import Optional

from mcp.server.fastmcp import FastMCP

from app.services.policy import get_domain_policy, list_address_policies
from app.services.actions import log_action
from app.services import compose as compose_svc
from app.services import start_here as start_here_svc
from app import drafts as drafts_module

mcp = FastMCP(
    "Envelope Email",
    instructions=(
        "Envelope is a programmable email API. "
        "Call start_here first with the account_id to get the domain policy and "
        "address policies that govern how this account should behave. "
        "Use the attribution schema returned by start_here when composing email. "
        "Do not infer or guess routing thresholds or modifier weights. "
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
async def compose_email(
    account_id: str,
    to: str,
    justification: str,
    confidence: Optional[float] = None,
    attribution: dict | None = None,
    subject: str = None,
    body: str = None,
    text: str = None,
    html: str = None,
    in_reply_to: str = None,
    created_by: str = "agent",
    cc: str = None,
    bcc: str = None,
    reply_to: str = None,
    attachments: list[dict] = None,
) -> str:
    """Compose an email and let the server route it.

    Preferred mode: provide attribution, not confidence. The server scores the
    attribution tags privately and decides whether to auto-send, queue for review,
    or block. Legacy confidence is still accepted for backward compatibility.

    If attribution is present, confidence is ignored.

    Set in_reply_to to the Message-ID of the email being replied to.
    created_by should be 'agent' for agent-created drafts.
    justification must explain why the email is safe to send or review.
    cc/bcc: comma-separated addresses. reply_to: single address.
    attachments: list of {filename, content (base64), content_type?, content_id?}
    """
    try:
        outcome = await compose_svc.route_composed_email(
            account_id=account_id,
            to_addr=to,
            confidence=confidence,
            attribution=attribution,
            justification=justification,
            subject=subject,
            text_content=text or body,
            html_content=html,
            in_reply_to=in_reply_to,
            created_by=created_by,
            cc_addr=cc,
            bcc_addr=bcc,
            reply_to=reply_to,
            attachments=attachments,
        )
    except compose_svc.DraftRoutingError as exc:
        error = {"error": exc.detail}
        if exc.error_type:
            error["error_type"] = exc.error_type
        return json.dumps(error)
    return json.dumps(outcome, indent=2)


@mcp.tool()
async def get_draft_tool(account_id: str, draft_id: str) -> str:
    """Get a draft by ID.

    Use to check the current state of a draft.
    """
    draft = await drafts_module.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        return json.dumps({"error": "Draft not found"})
    return json.dumps(draft, indent=2)


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
    if draft["status"] not in compose_svc.EDITABLE_DRAFT_STATUSES:
        return json.dumps({"error": f"Cannot reject draft with status '{draft['status']}'"})

    existing_meta = draft.get("metadata") or {}
    existing_meta["rejected_at"] = datetime.now(timezone.utc).isoformat()
    if feedback:
        existing_meta["rejection_feedback"] = feedback
    await drafts_module.update_draft(draft_id, metadata=existing_meta)
    await drafts_module.discard_draft(draft_id)
    return json.dumps({"status": "rejected", "id": draft_id})
