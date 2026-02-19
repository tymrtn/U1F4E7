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
async def test_send_returns_pending_not_success(client):
    """Resend is stripped. Send must NOT return status=success."""
    resp = await client.post("/send", json={
        "from_email": "a@b.com",
        "to": "c@d.com",
        "subject": "x",
        "text": "y",
    })
    body = resp.json()
    assert body["status"] != "success", "Send should be a stub, not a live transport"


@pytest.mark.asyncio
async def test_send_with_html(client):
    resp = await client.post("/send", json={
        "from_email": "a@b.com",
        "to": "c@d.com",
        "subject": "HTML test",
        "html": "<h1>Hi</h1>",
    })
    assert resp.status_code == 200
    assert resp.json()["status"] == "pending"


@pytest.mark.asyncio
async def test_send_missing_required_fields(client):
    resp = await client.post("/send", json={
        "from_email": "a@b.com",
    })
    assert resp.status_code == 422


@pytest.mark.asyncio
async def test_no_resend_webhook_endpoint(client):
    """Resend webhook endpoint must not exist."""
    resp = await client.post("/webhooks/resend")
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_no_resend_in_openapi_schema(client):
    """No trace of Resend in the API schema."""
    resp = await client.get("/openapi.json")
    assert resp.status_code == 200
    schema_text = resp.text.lower()
    assert "resend" not in schema_text
