# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import pytest
from unittest.mock import AsyncMock, patch


async def _create_account(client):
    resp = await client.post("/accounts", json={
        "name": "Draft Test",
        "host": "mail.example.com",
        "username": "test@example.com",
        "password": "secret",
    })
    return resp.json()


async def _create_draft(client, account_id, **overrides):
    payload = {
        "to": "recipient@example.com",
        "subject": "Test Draft",
        "text": "Hello from draft",
    }
    payload.update(overrides)
    resp = await client.post(f"/accounts/{account_id}/drafts", json=payload)
    return resp


# --- CRUD lifecycle ---


@pytest.mark.asyncio
async def test_create_draft(client):
    account = await _create_account(client)
    resp = await _create_draft(client, account["id"])
    assert resp.status_code == 201
    draft = resp.json()
    assert draft["status"] == "draft"
    assert draft["to_addr"] == "recipient@example.com"
    assert draft["subject"] == "Test Draft"
    assert draft["text_content"] == "Hello from draft"
    assert draft["account_id"] == account["id"]


@pytest.mark.asyncio
async def test_list_drafts(client):
    account = await _create_account(client)
    await _create_draft(client, account["id"], subject="Draft 1")
    await _create_draft(client, account["id"], subject="Draft 2")

    resp = await client.get(f"/accounts/{account['id']}/drafts")
    assert resp.status_code == 200
    items = resp.json()
    assert len(items) == 2


@pytest.mark.asyncio
async def test_list_drafts_pagination(client):
    account = await _create_account(client)
    for i in range(5):
        await _create_draft(client, account["id"], subject=f"Draft {i}")

    resp = await client.get(f"/accounts/{account['id']}/drafts?limit=2&offset=0")
    assert len(resp.json()) == 2

    resp = await client.get(f"/accounts/{account['id']}/drafts?limit=2&offset=3")
    assert len(resp.json()) == 2


@pytest.mark.asyncio
async def test_get_draft(client):
    account = await _create_account(client)
    created = (await _create_draft(client, account["id"])).json()

    resp = await client.get(f"/accounts/{account['id']}/drafts/{created['id']}")
    assert resp.status_code == 200
    assert resp.json()["id"] == created["id"]


@pytest.mark.asyncio
async def test_get_draft_not_found(client):
    account = await _create_account(client)
    resp = await client.get(f"/accounts/{account['id']}/drafts/nonexistent")
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_update_draft(client):
    account = await _create_account(client)
    created = (await _create_draft(client, account["id"])).json()

    resp = await client.put(
        f"/accounts/{account['id']}/drafts/{created['id']}",
        json={"subject": "Updated Subject", "to": "new@example.com"},
    )
    assert resp.status_code == 200
    updated = resp.json()
    assert updated["subject"] == "Updated Subject"
    assert updated["to_addr"] == "new@example.com"
    # Original field unchanged
    assert updated["text_content"] == "Hello from draft"


@pytest.mark.asyncio
async def test_discard_draft(client):
    account = await _create_account(client)
    created = (await _create_draft(client, account["id"])).json()

    resp = await client.delete(f"/accounts/{account['id']}/drafts/{created['id']}")
    assert resp.status_code == 200
    assert resp.json()["status"] == "discarded"

    # Verify status changed
    resp = await client.get(f"/accounts/{account['id']}/drafts/{created['id']}")
    assert resp.json()["status"] == "discarded"


# --- State machine ---


@pytest.mark.asyncio
async def test_update_after_discard_returns_409(client):
    account = await _create_account(client)
    created = (await _create_draft(client, account["id"])).json()
    await client.delete(f"/accounts/{account['id']}/drafts/{created['id']}")

    resp = await client.put(
        f"/accounts/{account['id']}/drafts/{created['id']}",
        json={"subject": "Nope"},
    )
    assert resp.status_code == 409


@pytest.mark.asyncio
async def test_discard_after_discard_returns_409(client):
    account = await _create_account(client)
    created = (await _create_draft(client, account["id"])).json()
    await client.delete(f"/accounts/{account['id']}/drafts/{created['id']}")

    resp = await client.delete(f"/accounts/{account['id']}/drafts/{created['id']}")
    assert resp.status_code == 409


@pytest.mark.asyncio
async def test_send_after_send_returns_409(client):
    account = await _create_account(client)
    created = (await _create_draft(client, account["id"])).json()

    with patch("app.main.send_message", new_callable=AsyncMock) as mock_send:
        mock_send.return_value = "<msg-id@example.com>"
        resp = await client.post(f"/accounts/{account['id']}/drafts/{created['id']}/send")
        assert resp.status_code == 200

        resp = await client.post(f"/accounts/{account['id']}/drafts/{created['id']}/send")
        assert resp.status_code == 409


# --- Metadata ---


@pytest.mark.asyncio
async def test_metadata_json_roundtrip(client):
    account = await _create_account(client)
    meta = {"agent": "inbox-bot", "thread_id": "abc123", "tags": ["urgent"]}
    resp = await _create_draft(client, account["id"], metadata=meta)
    draft = resp.json()
    assert draft["metadata"] == meta

    # Verify on get
    resp = await client.get(f"/accounts/{account['id']}/drafts/{draft['id']}")
    assert resp.json()["metadata"] == meta


@pytest.mark.asyncio
async def test_create_draft_with_created_by(client):
    account = await _create_account(client)
    resp = await _create_draft(client, account["id"], created_by="inbox-agent")
    assert resp.json()["created_by"] == "inbox-agent"


# --- Send integration ---


@pytest.mark.asyncio
async def test_send_draft_creates_message_record(client):
    account = await _create_account(client)
    created = (await _create_draft(client, account["id"])).json()

    with patch("app.main.send_message", new_callable=AsyncMock) as mock_send:
        mock_send.return_value = "<msg-id@example.com>"
        resp = await client.post(f"/accounts/{account['id']}/drafts/{created['id']}/send")
        assert resp.status_code == 200
        body = resp.json()
        assert body["status"] == "sent"
        assert body["draft_id"] == created["id"]
        assert "message_id" in body

        # Verify message record exists
        msg_resp = await client.get(f"/messages/{body['message_id']}")
        assert msg_resp.status_code == 200
        assert msg_resp.json()["status"] == "sent"


@pytest.mark.asyncio
async def test_send_draft_with_in_reply_to(client):
    account = await _create_account(client)
    resp = await _create_draft(
        client, account["id"],
        in_reply_to="<original@example.com>",
    )
    created = resp.json()

    with patch("app.main.send_message", new_callable=AsyncMock) as mock_send:
        mock_send.return_value = "<reply-id@example.com>"
        resp = await client.post(f"/accounts/{account['id']}/drafts/{created['id']}/send")
        assert resp.status_code == 200

        # Check that build_mime_message was called and In-Reply-To was set
        call_args = mock_send.call_args
        sent_msg = call_args[0][1]  # second positional arg is the EmailMessage
        assert sent_msg["In-Reply-To"] == "<original@example.com>"


@pytest.mark.asyncio
async def test_send_draft_smtp_error(client):
    from app.transport.smtp import SmtpSendError
    account = await _create_account(client)
    created = (await _create_draft(client, account["id"])).json()

    with patch("app.main.send_message", new_callable=AsyncMock) as mock_send:
        mock_send.side_effect = SmtpSendError("connection_error", "Connection refused")
        resp = await client.post(f"/accounts/{account['id']}/drafts/{created['id']}/send")
        assert resp.status_code == 502


@pytest.mark.asyncio
async def test_draft_for_nonexistent_account(client):
    resp = await client.post("/accounts/nonexistent/drafts", json={
        "to": "a@b.com", "subject": "Hi", "text": "body",
    })
    assert resp.status_code == 404


# --- Filters (Phase 1: Approval Gate) ---


@pytest.mark.asyncio
async def test_filter_drafts_by_status(client):
    account = await _create_account(client)
    aid = account["id"]

    d1 = (await _create_draft(client, aid, subject="Draft A")).json()
    d2 = (await _create_draft(client, aid, subject="Draft B")).json()

    # Discard one
    await client.delete(f"/accounts/{aid}/drafts/{d1['id']}")

    # Filter for active drafts
    resp = await client.get(f"/accounts/{aid}/drafts?status=draft")
    assert resp.status_code == 200
    items = resp.json()
    assert len(items) == 1
    assert items[0]["id"] == d2["id"]

    # Filter for discarded
    resp = await client.get(f"/accounts/{aid}/drafts?status=discarded")
    items = resp.json()
    assert len(items) == 1
    assert items[0]["id"] == d1["id"]


@pytest.mark.asyncio
async def test_filter_drafts_by_created_by(client):
    account = await _create_account(client)
    aid = account["id"]

    (await _create_draft(client, aid, subject="Human draft")).json()
    (await _create_draft(client, aid, subject="Agent draft", created_by="inbox-agent")).json()

    resp = await client.get(f"/accounts/{aid}/drafts?created_by=inbox-agent")
    items = resp.json()
    assert len(items) == 1
    assert items[0]["subject"] == "Agent draft"
    assert items[0]["created_by"] == "inbox-agent"


@pytest.mark.asyncio
async def test_filter_drafts_combined(client):
    account = await _create_account(client)
    aid = account["id"]

    d1 = (await _create_draft(client, aid, subject="Agent 1", created_by="inbox-agent")).json()
    (await _create_draft(client, aid, subject="Agent 2", created_by="inbox-agent")).json()
    (await _create_draft(client, aid, subject="Human 1")).json()

    await client.delete(f"/accounts/{aid}/drafts/{d1['id']}")

    resp = await client.get(f"/accounts/{aid}/drafts?status=draft&created_by=inbox-agent")
    items = resp.json()
    assert len(items) == 1
    assert items[0]["subject"] == "Agent 2"


# --- Approval metadata (Phase 1) ---


@pytest.mark.asyncio
async def test_send_with_approved_by_records_metadata(client):
    account = await _create_account(client)
    aid = account["id"]
    draft = (await _create_draft(client, aid, metadata={"agent": "inbox-agent"})).json()

    with patch("app.main.send_message", new_callable=AsyncMock) as mock_send:
        mock_send.return_value = "<sent@example.com>"
        resp = await client.post(f"/accounts/{aid}/drafts/{draft['id']}/send?approved_by=tyler")
        assert resp.status_code == 200

    from app.drafts import get_draft
    updated = await get_draft(draft["id"])
    assert updated["metadata"]["approved_by"] == "tyler"
    assert "approved_at" in updated["metadata"]
    assert updated["metadata"]["agent"] == "inbox-agent"


# --- Reject with feedback (Phase 1) ---


@pytest.mark.asyncio
async def test_reject_with_feedback(client):
    account = await _create_account(client)
    aid = account["id"]
    draft = (await _create_draft(client, aid, metadata={"agent": "inbox-agent"})).json()

    resp = await client.post(
        f"/accounts/{aid}/drafts/{draft['id']}/reject",
        json={"feedback": "Too formal, soften the tone"},
    )
    assert resp.status_code == 200
    assert resp.json()["status"] == "rejected"

    from app.drafts import get_draft
    updated = await get_draft(draft["id"])
    assert updated["status"] == "discarded"
    assert updated["metadata"]["rejection_feedback"] == "Too formal, soften the tone"
    assert "rejected_at" in updated["metadata"]


@pytest.mark.asyncio
async def test_reject_without_feedback(client):
    account = await _create_account(client)
    aid = account["id"]
    draft = (await _create_draft(client, aid)).json()

    resp = await client.post(
        f"/accounts/{aid}/drafts/{draft['id']}/reject",
        json={},
    )
    assert resp.status_code == 200
    assert resp.json()["status"] == "rejected"


@pytest.mark.asyncio
async def test_reject_already_sent_fails(client):
    account = await _create_account(client)
    aid = account["id"]
    draft = (await _create_draft(client, aid)).json()

    with patch("app.main.send_message", new_callable=AsyncMock) as mock_send:
        mock_send.return_value = "<sent@example.com>"
        await client.post(f"/accounts/{aid}/drafts/{draft['id']}/send")

    resp = await client.post(
        f"/accounts/{aid}/drafts/{draft['id']}/reject",
        json={"feedback": "Too late"},
    )
    assert resp.status_code == 409


@pytest.mark.asyncio
async def test_reject_nonexistent_draft(client):
    account = await _create_account(client)
    resp = await client.post(
        f"/accounts/{account['id']}/drafts/nonexistent/reject",
        json={},
    )
    assert resp.status_code == 404
