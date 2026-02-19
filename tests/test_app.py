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
async def test_send_stub_returns_pending(client):
    resp = await client.post("/send", json={
        "from_email": "agent@example.com",
        "to": "human@example.com",
        "subject": "Test",
        "text": "Hello",
    })
    assert resp.status_code == 200
    body = resp.json()
    assert body["status"] == "pending"
    assert body["envelope"]["from"] == "agent@example.com"
    assert body["envelope"]["to"] == "human@example.com"
    assert body["envelope"]["subject"] == "Test"


@pytest.mark.asyncio
async def test_send_missing_required_fields(client):
    resp = await client.post("/send", json={
        "from_email": "a@b.com",
    })
    assert resp.status_code == 422
