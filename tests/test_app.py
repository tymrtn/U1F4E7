import pytest


@pytest.mark.asyncio
async def test_dashboard_returns_html(client):
    resp = await client.get("/")
    assert resp.status_code == 200
    assert "text/html" in resp.headers["content-type"]


@pytest.mark.asyncio
async def test_openapi_schema_accessible(client):
    resp = await client.get("/openapi.json")
    assert resp.status_code == 200
    schema = resp.json()
    assert schema["info"]["title"] == "Envelope Email API"


@pytest.mark.asyncio
async def test_send_requires_account_id(client):
    resp = await client.post("/send", json={
        "to": "human@example.com",
        "subject": "Test",
        "text": "Hello",
    })
    assert resp.status_code == 422


@pytest.mark.asyncio
async def test_send_unknown_account_returns_404(client):
    resp = await client.post("/send", json={
        "account_id": "nonexistent",
        "to": "human@example.com",
        "subject": "Test",
        "text": "Hello",
    })
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_send_missing_required_fields(client):
    resp = await client.post("/send", json={
        "account_id": "some-id",
    })
    assert resp.status_code == 422


@pytest.mark.asyncio
async def test_account_crud(client):
    # Create
    resp = await client.post("/accounts", json={
        "name": "Test Account",
        "host": "mail.example.com",
        "username": "test@example.com",
        "password": "secret",
    })
    assert resp.status_code == 200
    account = resp.json()
    assert account["name"] == "Test Account"
    account_id = account["id"]

    # List
    resp = await client.get("/accounts")
    assert resp.status_code == 200
    accounts = resp.json()
    assert len(accounts) >= 1
    assert any(a["id"] == account_id for a in accounts)

    # Get
    resp = await client.get(f"/accounts/{account_id}")
    assert resp.status_code == 200
    assert resp.json()["id"] == account_id

    # Delete
    resp = await client.delete(f"/accounts/{account_id}")
    assert resp.status_code == 200
    assert resp.json()["status"] == "deleted"

    # Verify deleted
    resp = await client.get(f"/accounts/{account_id}")
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_messages_list_empty(client):
    resp = await client.get("/messages")
    assert resp.status_code == 200
    assert resp.json() == []


@pytest.mark.asyncio
async def test_message_not_found(client):
    resp = await client.get("/messages/nonexistent")
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_stats_empty(client):
    resp = await client.get("/stats")
    assert resp.status_code == 200
    stats = resp.json()
    assert stats["total"] == 0
    assert stats["sent"] == 0
    assert stats["failed"] == 0
    assert stats["success_rate"] == 0


# --- Story 006: New endpoint tests ---


async def _create_test_account(client):
    resp = await client.post("/accounts", json={
        "name": "Test Account",
        "host": "mail.example.com",
        "username": "test@example.com",
        "password": "secret",
    })
    return resp.json()


@pytest.mark.asyncio
async def test_send_wait_false_returns_queued(client):
    account = await _create_test_account(client)
    resp = await client.post("/send", json={
        "account_id": account["id"],
        "to": "recipient@example.com",
        "subject": "Async test",
        "text": "Hello",
        "wait": False,
    })
    assert resp.status_code == 200
    body = resp.json()
    assert body["status"] == "queued"
    assert "id" in body


@pytest.mark.asyncio
async def test_send_wait_false_calls_worker_notify(client):
    from app.main import app as the_app
    account = await _create_test_account(client)
    await client.post("/send", json={
        "account_id": account["id"],
        "to": "recipient@example.com",
        "subject": "Async test",
        "text": "Hello",
        "wait": False,
    })
    the_app.state.send_worker.notify.assert_called()


@pytest.mark.asyncio
async def test_send_wait_false_creates_message_record(client):
    account = await _create_test_account(client)
    resp = await client.post("/send", json={
        "account_id": account["id"],
        "to": "recipient@example.com",
        "subject": "Async test",
        "text": "Hello",
        "wait": False,
    })
    msg_id = resp.json()["id"]
    resp = await client.get(f"/messages/{msg_id}")
    assert resp.status_code == 200
    assert resp.json()["status"] == "queued"


@pytest.mark.asyncio
async def test_delete_account_invalidates_pool(client):
    from app.main import app as the_app
    account = await _create_test_account(client)
    initial_version = the_app.state.smtp_pool._credential_versions.get(account["id"], 0)
    await client.delete(f"/accounts/{account['id']}")
    new_version = the_app.state.smtp_pool._credential_versions.get(account["id"], 0)
    assert new_version > initial_version


@pytest.mark.asyncio
async def test_discover_stream_returns_sse_content_type(client):
    resp = await client.get("/accounts/discover/stream?email=test@example.com")
    assert "text/event-stream" in resp.headers["content-type"]


# --- Phase 2: Thread endpoint ---


@pytest.mark.asyncio
async def test_thread_endpoint_not_found_account(client):
    resp = await client.get("/accounts/nonexistent/threads/%3Cmsg@test.com%3E")
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_thread_endpoint_returns_structure(client):
    from unittest.mock import patch, AsyncMock
    account = await _create_test_account(client)
    mock_thread = [
        {"uid": "1", "message_id": "<a@test.com>", "subject": "Hello", "from_addr": "a@x.com",
         "to_addr": "b@x.com", "date": "Mon, 1 Jan 2026 12:00:00 +0000",
         "text_body": "Hi", "html_body": None, "in_reply_to": None, "references": None},
    ]
    with patch("app.main.get_thread", new_callable=AsyncMock, return_value=mock_thread):
        resp = await client.get(
            f"/accounts/{account['id']}/threads/%3Ca@test.com%3E"
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["count"] == 1
        assert len(data["thread"]) == 1


# --- Phase 3: Context endpoint ---


@pytest.mark.asyncio
async def test_context_endpoint_not_found_account(client):
    resp = await client.get("/accounts/nonexistent/context?q=test")
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_context_endpoint_returns_structure(client):
    from unittest.mock import patch, AsyncMock
    account = await _create_test_account(client)
    with patch(
        "app.embeddings.find_similar",
        new_callable=AsyncMock,
        return_value=[{"message_id": "<a@test.com>", "score": 0.95, "subject": "", "from_addr": "", "date": "", "preview": ""}],
    ):
        resp = await client.get(f"/accounts/{account['id']}/context?q=villa+pricing")
        assert resp.status_code == 200
        data = resp.json()
        assert data["query"] == "villa pricing"
        assert data["count"] == 1
        assert len(data["results"]) == 1


# --- Pre-marketing improvements ---


@pytest.mark.asyncio
async def test_send_display_name_override(client):
    """display_name on SendEmail body overrides the account-level display name."""
    from unittest.mock import patch, AsyncMock
    account = await _create_test_account(client)

    captured = {}

    async def fake_send(account, msg, pool=None):
        captured["from"] = msg.get("From")
        return "<fake-id@test>"

    with patch("app.main.send_message", new_callable=AsyncMock, side_effect=fake_send):
        resp = await client.post("/send", json={
            "account_id": account["id"],
            "to": "to@example.com",
            "subject": "Override test",
            "text": "hi",
            "display_name": "Alerts Bot",
        })
    assert resp.status_code == 200
    assert "Alerts Bot" in captured["from"]


@pytest.mark.asyncio
async def test_send_display_name_falls_back_to_account(client):
    """When no display_name in body, account default is used."""
    from unittest.mock import patch, AsyncMock
    resp = await client.post("/accounts", json={
        "name": "Named Account",
        "host": "mail.example.com",
        "username": "test@example.com",
        "password": "secret",
        "display_name": "Default Name",
    })
    account = resp.json()

    captured = {}

    async def fake_send(account, msg, pool=None):
        captured["from"] = msg.get("From")
        return "<fake-id@test>"

    with patch("app.main.send_message", new_callable=AsyncMock, side_effect=fake_send):
        resp = await client.post("/send", json={
            "account_id": account["id"],
            "to": "to@example.com",
            "subject": "Fallback test",
            "text": "hi",
        })
    assert resp.status_code == 200
    assert "Default Name" in captured["from"]


@pytest.mark.asyncio
async def test_rate_limit_blocks_after_cap(client):
    """3rd send within the hour returns 429 when rate_limit_per_hour=2."""
    resp = await client.post("/accounts", json={
        "name": "Limited Account",
        "host": "mail.example.com",
        "username": "test@example.com",
        "password": "secret",
        "rate_limit_per_hour": 2,
    })
    account = resp.json()
    assert account.get("rate_limit_per_hour") == 2

    payload = {
        "account_id": account["id"],
        "to": "to@example.com",
        "subject": "Rate test",
        "text": "hi",
        "wait": False,
    }
    resp1 = await client.post("/send", json=payload)
    assert resp1.status_code == 200
    resp2 = await client.post("/send", json=payload)
    assert resp2.status_code == 200
    resp3 = await client.post("/send", json=payload)
    assert resp3.status_code == 429
    body = resp3.json()
    assert body["error"] == "rate_limit_exceeded"
    assert body["limit"] == 2


@pytest.mark.asyncio
async def test_rate_limit_not_applied_when_unset(client):
    """Accounts without a rate limit can send freely."""
    account = await _create_test_account(client)
    payload = {
        "account_id": account["id"],
        "to": "to@example.com",
        "subject": "No limit test",
        "text": "hi",
        "wait": False,
    }
    for _ in range(5):
        resp = await client.post("/send", json=payload)
        assert resp.status_code == 200


def test_parse_message_id_strips_whitespace():
    """_parse_message_id trims leading/trailing whitespace from the header value."""
    import email as email_module
    from app.transport.imap import _parse_message_id

    raw = email_module.message_from_string(
        "Message-ID:  <test@host.com>  \r\nFrom: a@b.com\r\n\r\n"
    )
    result = _parse_message_id(raw)
    assert result == "<test@host.com>"


def test_parse_message_id_returns_none_when_absent():
    """_parse_message_id returns None when the header is missing."""
    import email as email_module
    from app.transport.imap import _parse_message_id

    raw = email_module.message_from_string("From: a@b.com\r\n\r\n")
    assert _parse_message_id(raw) is None


# --- Story 011: Domain Policy CRUD ---


@pytest.mark.asyncio
async def test_domain_policy_upsert_and_get(client):
    account = await _create_test_account(client)
    aid = account["id"]

    # Upsert
    resp = await client.post(f"/accounts/{aid}/domain-policy", json={
        "name": "Support Policy",
        "description": "Handle support emails",
        "values": ["empathy", "speed"],
        "tone": "friendly",
        "style": "brief",
    })
    assert resp.status_code == 200
    data = resp.json()
    assert data["name"] == "Support Policy"
    assert data["values"] == ["empathy", "speed"]

    # Get
    resp = await client.get(f"/accounts/{aid}/domain-policy")
    assert resp.status_code == 200
    assert resp.json()["tone"] == "friendly"


@pytest.mark.asyncio
async def test_domain_policy_404_if_missing(client):
    account = await _create_test_account(client)
    resp = await client.get(f"/accounts/{account['id']}/domain-policy")
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_domain_policy_upsert_unknown_account(client):
    resp = await client.post("/accounts/nonexistent/domain-policy", json={"name": "X"})
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_address_policy_crud(client):
    account = await _create_test_account(client)
    aid = account["id"]

    # Create
    resp = await client.post(f"/accounts/{aid}/address-policies", json={
        "pattern": "*@vip.com",
        "purpose": "VIP customers",
        "confidence_threshold": 0.9,
    })
    assert resp.status_code == 201
    data = resp.json()
    assert data["pattern"] == "*@vip.com"
    assert data["confidence_threshold"] == 0.9

    # List
    resp = await client.get(f"/accounts/{aid}/address-policies")
    assert resp.status_code == 200
    policies = resp.json()
    assert len(policies) == 1
    assert policies[0]["pattern"] == "*@vip.com"

    # Get
    resp = await client.get(f"/accounts/{aid}/address-policies/*@vip.com")
    assert resp.status_code == 200
    assert resp.json()["purpose"] == "VIP customers"

    # Update (PUT)
    resp = await client.put(f"/accounts/{aid}/address-policies/*@vip.com", json={
        "pattern": "*@vip.com",
        "purpose": "Updated purpose",
        "confidence_threshold": 0.8,
    })
    assert resp.status_code == 200
    assert resp.json()["purpose"] == "Updated purpose"

    # Delete
    resp = await client.delete(f"/accounts/{aid}/address-policies/*@vip.com")
    assert resp.status_code == 200
    assert resp.json()["status"] == "deleted"

    # Verify gone
    resp = await client.get(f"/accounts/{aid}/address-policies/*@vip.com")
    assert resp.status_code == 404


# --- Story 012: Start Here ---


@pytest.mark.asyncio
async def test_start_here_onboarding_mode(client):
    account = await _create_test_account(client)
    resp = await client.get(f"/accounts/{account['id']}/start-here")
    assert resp.status_code == 200
    data = resp.json()
    assert data["mode"] == "onboarding"
    assert "instructions" in data
    assert "POST" in data["instructions"]


@pytest.mark.asyncio
async def test_start_here_operational_mode(client):
    account = await _create_test_account(client)
    aid = account["id"]

    await client.post(f"/accounts/{aid}/domain-policy", json={
        "name": "My Policy",
        "tone": "neutral",
    })

    resp = await client.get(f"/accounts/{aid}/start-here")
    assert resp.status_code == 200
    data = resp.json()
    assert data["mode"] == "operational"
    assert "domain_policy" in data
    assert data["domain_policy"]["tone"] == "neutral"


@pytest.mark.asyncio
async def test_start_here_no_auth_required(client):
    """start-here must be accessible without API key (when key is set)."""
    import os
    from app.main import app as the_app
    account = await _create_test_account(client)
    # Temporarily set API key enforcement
    import app.main as main_module
    original = main_module._api_key
    main_module._api_key = "test-secret-key"
    try:
        resp = await client.get(f"/accounts/{account['id']}/start-here")
        assert resp.status_code == 200
    finally:
        main_module._api_key = original


# --- Story 013: Action Log ---


@pytest.mark.asyncio
async def test_log_action_and_retrieve(client):
    account = await _create_test_account(client)
    aid = account["id"]

    resp = await client.post("/actions/log", json={
        "account_id": aid,
        "action_type": "inbound_route",
        "confidence": 0.85,
        "justification": "Matched VIP pattern",
        "action_taken": "Created draft reply",
    })
    assert resp.status_code == 201
    entry = resp.json()
    assert entry["action_type"] == "inbound_route"
    assert entry["confidence"] == 0.85
    log_id = entry["id"]

    # Get by ID
    resp = await client.get(f"/actions/log/{log_id}")
    assert resp.status_code == 200
    assert resp.json()["justification"] == "Matched VIP pattern"


@pytest.mark.asyncio
async def test_list_actions_for_account(client):
    account = await _create_test_account(client)
    aid = account["id"]

    for i in range(3):
        await client.post("/actions/log", json={
            "account_id": aid,
            "action_type": "trash",
            "confidence": 0.95,
            "justification": f"Spam #{i}",
            "action_taken": "Discarded",
        })

    resp = await client.get(f"/accounts/{aid}/actions")
    assert resp.status_code == 200
    entries = resp.json()
    assert len(entries) == 3


@pytest.mark.asyncio
async def test_action_log_invalid_type(client):
    account = await _create_test_account(client)
    resp = await client.post("/actions/log", json={
        "account_id": account["id"],
        "action_type": "invalid_type",
        "confidence": 0.5,
        "justification": "test",
        "action_taken": "test",
    })
    assert resp.status_code == 422


@pytest.mark.asyncio
async def test_action_log_not_found(client):
    resp = await client.get("/actions/log/nonexistent-id")
    assert resp.status_code == 404


# --- Story 017: Cc, Bcc, Reply-To ---


@pytest.mark.asyncio
async def test_send_with_cc_sets_header(client):
    from unittest.mock import patch, AsyncMock
    from app.transport.smtp import build_mime_message as real_build
    account = await _create_test_account(client)
    captured = {}

    def capturing_build(*args, **kwargs):
        msg = real_build(*args, **kwargs)
        captured["msg"] = msg
        return msg

    with patch("app.main.build_mime_message", side_effect=capturing_build), \
         patch("app.main.send_message", new_callable=AsyncMock, return_value="<mid@example.com>"):
        resp = await client.post("/send", json={
            "account_id": account["id"],
            "to": "to@example.com",
            "cc": "cc@example.com",
            "subject": "CC Test",
            "text": "Hello",
        })
    assert resp.status_code == 200
    assert captured["msg"]["Cc"] == "cc@example.com"


@pytest.mark.asyncio
async def test_send_with_reply_to(client):
    from unittest.mock import patch, AsyncMock
    from app.transport.smtp import build_mime_message as real_build
    account = await _create_test_account(client)
    captured = {}

    def capturing_build(*args, **kwargs):
        msg = real_build(*args, **kwargs)
        captured["msg"] = msg
        return msg

    with patch("app.main.build_mime_message", side_effect=capturing_build), \
         patch("app.main.send_message", new_callable=AsyncMock, return_value="<mid@example.com>"):
        resp = await client.post("/send", json={
            "account_id": account["id"],
            "to": "to@example.com",
            "reply_to": "noreply@example.com",
            "subject": "Reply-To Test",
            "text": "Hello",
        })
    assert resp.status_code == 200
    assert captured["msg"]["Reply-To"] == "noreply@example.com"


@pytest.mark.asyncio
async def test_draft_cc_bcc_persisted(client):
    account = await _create_test_account(client)
    resp = await client.post(f"/accounts/{account['id']}/drafts", json={
        "to": "to@example.com",
        "subject": "CC Draft",
        "text": "Body",
        "cc": "cc@example.com",
        "bcc": "bcc@example.com",
        "reply_to": "reply@example.com",
    })
    assert resp.status_code == 201
    draft = resp.json()
    assert draft["cc_addr"] == "cc@example.com"
    assert draft["bcc_addr"] == "bcc@example.com"
    assert draft["reply_to"] == "reply@example.com"

    # Verify persisted via GET
    resp2 = await client.get(f"/accounts/{account['id']}/drafts/{draft['id']}")
    assert resp2.status_code == 200
    fetched = resp2.json()
    assert fetched["cc_addr"] == "cc@example.com"
    assert fetched["bcc_addr"] == "bcc@example.com"
    assert fetched["reply_to"] == "reply@example.com"


# --- Story 018: Account Signatures ---


@pytest.mark.asyncio
async def test_signature_appended_to_text(client):
    from unittest.mock import patch, AsyncMock
    from app.transport.smtp import build_mime_message as real_build
    account = await _create_test_account(client)

    # Set signature
    resp = await client.patch(f"/accounts/{account['id']}", json={
        "signature_text": "Best,\nAlice",
    })
    assert resp.status_code == 200
    assert resp.json()["signature_text"] == "Best,\nAlice"

    captured = {}

    def capturing_build(*args, **kwargs):
        captured["kwargs"] = kwargs
        return real_build(*args, **kwargs)

    with patch("app.main.build_mime_message", side_effect=capturing_build), \
         patch("app.main.send_message", new_callable=AsyncMock, return_value="<mid@example.com>"):
        resp = await client.post("/send", json={
            "account_id": account["id"],
            "to": "to@example.com",
            "subject": "Sig Test",
            "text": "Hello there",
        })
    assert resp.status_code == 200
    text_sent = captured["kwargs"]["text"]
    assert "Best,\nAlice" in text_sent
    assert text_sent.startswith("Hello there")


@pytest.mark.asyncio
async def test_signature_injected_into_html(client):
    from unittest.mock import patch, AsyncMock
    from app.transport.smtp import build_mime_message as real_build
    account = await _create_test_account(client)

    await client.patch(f"/accounts/{account['id']}", json={
        "signature_html": "<p>Best, Alice</p>",
    })

    captured = {}

    def capturing_build(*args, **kwargs):
        captured["kwargs"] = kwargs
        return real_build(*args, **kwargs)

    with patch("app.main.build_mime_message", side_effect=capturing_build), \
         patch("app.main.send_message", new_callable=AsyncMock, return_value="<mid@example.com>"):
        resp = await client.post("/send", json={
            "account_id": account["id"],
            "to": "to@example.com",
            "subject": "HTML Sig Test",
            "html": "<html><body><p>Hello</p></body></html>",
        })
    assert resp.status_code == 200
    html_sent = captured["kwargs"]["html"]
    assert '<div class="env-signature"><p>Best, Alice</p></div>' in html_sent
    assert html_sent.index('<div class="env-signature">') < html_sent.index("</body>")


@pytest.mark.asyncio
async def test_use_signature_false_skips_injection(client):
    from unittest.mock import patch, AsyncMock
    from app.transport.smtp import build_mime_message as real_build
    account = await _create_test_account(client)

    await client.patch(f"/accounts/{account['id']}", json={
        "signature_text": "Best,\nAlice",
    })

    captured = {}

    def capturing_build(*args, **kwargs):
        captured["kwargs"] = kwargs
        return real_build(*args, **kwargs)

    with patch("app.main.build_mime_message", side_effect=capturing_build), \
         patch("app.main.send_message", new_callable=AsyncMock, return_value="<mid@example.com>"):
        resp = await client.post("/send", json={
            "account_id": account["id"],
            "to": "to@example.com",
            "subject": "No Sig",
            "text": "Hello there",
            "use_signature": False,
        })
    assert resp.status_code == 200
    text_sent = captured["kwargs"]["text"]
    assert "Best,\nAlice" not in text_sent


# --- Story 019: Open Tracking ---


@pytest.mark.asyncio
async def test_tracking_pixel_injected_into_html(client):
    from unittest.mock import patch, AsyncMock
    from app.transport.smtp import build_mime_message as real_build
    account = await _create_test_account(client)

    captured = {}

    def capturing_build(*args, **kwargs):
        captured["kwargs"] = kwargs
        return real_build(*args, **kwargs)

    with patch("app.main.build_mime_message", side_effect=capturing_build), \
         patch("app.main.send_message", new_callable=AsyncMock, return_value="<mid@example.com>"):
        resp = await client.post("/send", json={
            "account_id": account["id"],
            "to": "to@example.com",
            "subject": "Track Test",
            "html": "<html><body><p>Hello</p></body></html>",
            "track_opens": True,
        })
    assert resp.status_code == 200
    html_sent = captured["kwargs"]["html"]
    assert '<img src=' in html_sent
    assert '/track/' in html_sent
    assert 'width="1"' in html_sent


@pytest.mark.asyncio
async def test_track_endpoint_returns_gif(client):
    resp = await client.get("/track/unknown-token-xyz")
    assert resp.status_code == 200
    assert "image/gif" in resp.headers["content-type"]


@pytest.mark.asyncio
async def test_track_endpoint_records_open(client):
    from unittest.mock import patch, AsyncMock
    account = await _create_test_account(client)

    msg_id_ref = {}

    with patch("app.main.send_message", new_callable=AsyncMock, return_value="<mid@example.com>"):
        resp = await client.post("/send", json={
            "account_id": account["id"],
            "to": "to@example.com",
            "subject": "Track Open",
            "html": "<p>Hello</p>",
            "track_opens": True,
        })
    assert resp.status_code == 200
    msg_id_ref["id"] = resp.json()["id"]

    # Grab the tracking token from messages table
    from app.db import get_db
    db = await get_db()
    cursor = await db.execute("SELECT tracking_token FROM messages WHERE id = ?", (msg_id_ref["id"],))
    row = await cursor.fetchone()
    assert row is not None
    token = row["tracking_token"]
    assert token is not None

    # Simulate open
    resp2 = await client.get(f"/track/{token}")
    assert resp2.status_code == 200
    assert "image/gif" in resp2.headers["content-type"]

    # Verify open recorded
    resp3 = await client.get(f"/messages/{msg_id_ref['id']}/opens")
    assert resp3.status_code == 200
    opens = resp3.json()
    assert len(opens) == 1
    assert opens[0]["token"] == token


@pytest.mark.asyncio
async def test_message_opens_list(client):
    from unittest.mock import patch, AsyncMock
    account = await _create_test_account(client)

    with patch("app.main.send_message", new_callable=AsyncMock, return_value="<mid@example.com>"):
        resp = await client.post("/send", json={
            "account_id": account["id"],
            "to": "to@example.com",
            "subject": "Open List Test",
            "html": "<p>Hello</p>",
            "track_opens": True,
        })
    msg_id = resp.json()["id"]

    from app.db import get_db
    db = await get_db()
    cursor = await db.execute("SELECT tracking_token FROM messages WHERE id = ?", (msg_id,))
    row = await cursor.fetchone()
    token = row["tracking_token"]

    # Two opens
    await client.get(f"/track/{token}")
    await client.get(f"/track/{token}")

    resp = await client.get(f"/messages/{msg_id}/opens")
    assert resp.status_code == 200
    opens = resp.json()
    assert len(opens) == 2
