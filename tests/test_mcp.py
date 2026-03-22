# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import json
import pytest


@pytest.mark.asyncio
async def test_mcp_start_here_onboarding(setup_db):
    """start_here tool returns onboarding mode when no policy set."""
    pytest.importorskip("mcp")
    from app.mcp import start_here
    from app.credentials.store import create_account

    account = await create_account(
        name="MCP Test",
        smtp_host="smtp.example.com",
        smtp_port=587,
        imap_host="imap.example.com",
        imap_port=993,
        username="user@example.com",
        password="secret",
    )

    result_json = await start_here(account["id"])
    result = json.loads(result_json)
    assert result["mode"] == "onboarding"
    assert "instructions" in result


@pytest.mark.asyncio
async def test_mcp_log_action_tool(setup_db):
    """log_action_tool creates an action log entry."""
    pytest.importorskip("mcp")
    from app.mcp import log_action_tool
    from app.credentials.store import create_account

    account = await create_account(
        name="MCP Action Test",
        smtp_host="smtp.example.com",
        smtp_port=587,
        imap_host="imap.example.com",
        imap_port=993,
        username="user@example.com",
        password="secret",
    )

    result_json = await log_action_tool(
        account_id=account["id"],
        action_type="inbound_route",
        confidence=0.9,
        justification="Test justification",
        action_taken="Test action",
    )
    result = json.loads(result_json)
    assert result["action_type"] == "inbound_route"
    assert result["confidence"] == 0.9
    assert "id" in result


@pytest.mark.asyncio
async def test_mcp_create_and_reject_draft(setup_db):
    """create_draft_tool and reject_draft_tool work end-to-end."""
    pytest.importorskip("mcp")
    from app.mcp import create_draft_tool, reject_draft_tool
    from app.credentials.store import create_account

    account = await create_account(
        name="Draft MCP Test",
        smtp_host="smtp.example.com",
        smtp_port=587,
        imap_host="imap.example.com",
        imap_port=993,
        username="user@example.com",
        password="secret",
    )

    draft_json = await create_draft_tool(
        account_id=account["id"],
        to="recipient@example.com",
        subject="Test subject",
        text="Hello from agent",
    )
    draft = json.loads(draft_json)
    assert draft["status"] == "draft"
    draft_id = draft["id"]

    reject_json = await reject_draft_tool(
        account_id=account["id"],
        draft_id=draft_id,
        feedback="Not needed",
    )
    result = json.loads(reject_json)
    assert result["status"] == "rejected"


@pytest.mark.asyncio
async def test_mcp_get_domain_policy_missing(setup_db):
    """get_domain_policy_tool returns error dict when no policy exists."""
    pytest.importorskip("mcp")
    from app.mcp import get_domain_policy_tool
    from app.credentials.store import create_account

    account = await create_account(
        name="Policy MCP Test",
        smtp_host="smtp.example.com",
        smtp_port=587,
        imap_host="imap.example.com",
        imap_port=993,
        username="user@example.com",
        password="secret",
    )

    result_json = await get_domain_policy_tool(account["id"])
    result = json.loads(result_json)
    assert "error" in result
