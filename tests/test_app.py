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
