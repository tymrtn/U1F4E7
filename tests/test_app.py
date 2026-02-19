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
