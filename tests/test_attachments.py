# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import base64
import pytest
from unittest.mock import patch, AsyncMock

from app.transport.smtp import build_mime_message


# --- Helpers ---

def _b64(data: bytes) -> str:
    return base64.b64encode(data).decode()


async def _create_test_account(client):
    resp = await client.post("/accounts", json={
        "name": "Test Account",
        "host": "mail.example.com",
        "username": "test@example.com",
        "password": "secret",
    })
    return resp.json()


# --- Unit: build_mime_message ---


def test_build_mime_message_with_attachment():
    """MIME message includes an attachment part with correct filename."""
    att = {
        "filename": "invoice.pdf",
        "content": _b64(b"%PDF-1.4 fake content"),
        "content_type": "application/pdf",
    }
    msg = build_mime_message(
        from_addr="me@example.com",
        to_addr="you@example.com",
        subject="Test",
        text="See attached",
        attachments=[att],
    )
    # Walk MIME parts and find the attachment
    parts = list(msg.walk())
    attachment_parts = [p for p in parts if p.get_content_disposition() == "attachment"]
    assert len(attachment_parts) == 1
    assert attachment_parts[0].get_filename() == "invoice.pdf"
    assert attachment_parts[0].get_content_type() == "application/pdf"


def test_build_mime_message_with_inline_cid():
    """Attachment with content_id gets inline disposition and Content-ID header."""
    att = {
        "filename": "logo.png",
        "content": _b64(b"\x89PNG fake"),
        "content_type": "image/png",
        "content_id": "logo@envelope",
    }
    msg = build_mime_message(
        from_addr="me@example.com",
        to_addr="you@example.com",
        subject="Inline Test",
        html='<img src="cid:logo@envelope">',
        attachments=[att],
    )
    parts = list(msg.walk())
    inline_parts = [p for p in parts if p.get_content_disposition() == "inline"]
    assert len(inline_parts) == 1
    assert inline_parts[0].get_filename() == "logo.png"
    # Content-ID header should be present
    cid = inline_parts[0]["Content-ID"]
    assert "logo@envelope" in cid


def test_build_mime_message_guesses_content_type():
    """When content_type is omitted, it's guessed from the filename."""
    att = {
        "filename": "data.csv",
        "content": _b64(b"a,b,c\n1,2,3"),
    }
    msg = build_mime_message(
        from_addr="me@example.com",
        to_addr="you@example.com",
        subject="Guess Type",
        text="data attached",
        attachments=[att],
    )
    parts = list(msg.walk())
    attachment_parts = [p for p in parts if p.get_content_disposition() in ("attachment", "inline")]
    assert len(attachment_parts) == 1
    assert attachment_parts[0].get_content_type() == "text/csv"


def test_build_mime_message_no_attachments_unchanged():
    """When attachments is None/empty, message has no attachment parts."""
    msg = build_mime_message(
        from_addr="me@example.com",
        to_addr="you@example.com",
        subject="Plain",
        text="No attachments",
        attachments=None,
    )
    parts = list(msg.walk())
    attachment_parts = [p for p in parts if p.get_content_disposition() in ("attachment", "inline")]
    assert len(attachment_parts) == 0


def test_build_mime_message_multiple_attachments():
    """Multiple attachments all appear in the MIME message."""
    atts = [
        {"filename": "a.txt", "content": _b64(b"aaa"), "content_type": "text/plain"},
        {"filename": "b.txt", "content": _b64(b"bbb"), "content_type": "text/plain"},
    ]
    msg = build_mime_message(
        from_addr="me@example.com",
        to_addr="you@example.com",
        subject="Multi",
        text="Two files",
        attachments=atts,
    )
    parts = list(msg.walk())
    attachment_parts = [p for p in parts if p.get_content_disposition() in ("attachment", "inline")]
    assert len(attachment_parts) == 2
    filenames = {p.get_filename() for p in attachment_parts}
    assert filenames == {"a.txt", "b.txt"}


# --- Integration: REST API ---


@pytest.mark.asyncio
async def test_send_with_attachments(client):
    """POST /send with attachments passes them through to MIME builder."""
    account = await _create_test_account(client)
    captured = {}

    def capturing_build(*args, **kwargs):
        captured["kwargs"] = kwargs
        return build_mime_message(*args, **kwargs)

    with patch("app.main.build_mime_message", side_effect=capturing_build), \
         patch("app.main.send_message", new_callable=AsyncMock, return_value="<mid@test>"):
        resp = await client.post("/send", json={
            "account_id": account["id"],
            "to": "to@example.com",
            "subject": "Attachment Send",
            "text": "See attached",
            "attachments": [{
                "filename": "test.pdf",
                "content": _b64(b"fake pdf bytes"),
                "content_type": "application/pdf",
            }],
        })
    assert resp.status_code == 200
    assert resp.json()["status"] == "sent"
    assert captured["kwargs"]["attachments"] is not None
    assert len(captured["kwargs"]["attachments"]) == 1
    assert captured["kwargs"]["attachments"][0]["filename"] == "test.pdf"


@pytest.mark.asyncio
async def test_attachment_size_limit(client):
    """Total attachment size over 40 MB returns 422."""
    account = await _create_test_account(client)
    # 41 MB of data
    big_content = _b64(b"x" * (41 * 1024 * 1024))

    resp = await client.post("/send", json={
        "account_id": account["id"],
        "to": "to@example.com",
        "subject": "Too Big",
        "text": "oversized",
        "attachments": [{
            "filename": "huge.bin",
            "content": big_content,
        }],
    })
    assert resp.status_code == 422
    assert "40 MB" in resp.json()["detail"]


@pytest.mark.asyncio
async def test_draft_with_attachments_roundtrip(client):
    """Create draft with attachments, retrieve, update, and verify persistence."""
    account = await _create_test_account(client)
    aid = account["id"]

    att = {
        "filename": "notes.txt",
        "content": _b64(b"some notes"),
        "content_type": "text/plain",
    }

    # Create
    resp = await client.post(f"/accounts/{aid}/drafts", json={
        "to": "to@example.com",
        "subject": "Draft with attachment",
        "text": "See notes",
        "attachments": [att],
    })
    assert resp.status_code == 201
    draft = resp.json()
    assert len(draft["attachments"]) == 1
    assert draft["attachments"][0]["filename"] == "notes.txt"

    # Get
    resp = await client.get(f"/accounts/{aid}/drafts/{draft['id']}")
    assert resp.status_code == 200
    fetched = resp.json()
    assert len(fetched["attachments"]) == 1

    # Update attachments
    new_att = {
        "filename": "updated.txt",
        "content": _b64(b"updated notes"),
        "content_type": "text/plain",
    }
    resp = await client.put(f"/accounts/{aid}/drafts/{draft['id']}", json={
        "attachments": [new_att],
    })
    assert resp.status_code == 200
    updated = resp.json()
    assert len(updated["attachments"]) == 1
    assert updated["attachments"][0]["filename"] == "updated.txt"


@pytest.mark.asyncio
async def test_draft_send_with_attachments(client):
    """Sending a draft with attachments passes them to MIME builder."""
    account = await _create_test_account(client)
    aid = account["id"]

    att = {
        "filename": "report.pdf",
        "content": _b64(b"PDF data here"),
        "content_type": "application/pdf",
    }

    # Create draft
    resp = await client.post(f"/accounts/{aid}/drafts", json={
        "to": "to@example.com",
        "subject": "Draft Send",
        "text": "Report attached",
        "attachments": [att],
    })
    draft_id = resp.json()["id"]

    captured = {}

    def capturing_build(*args, **kwargs):
        captured["kwargs"] = kwargs
        return build_mime_message(*args, **kwargs)

    with patch("app.main.build_mime_message", side_effect=capturing_build), \
         patch("app.main.send_message", new_callable=AsyncMock, return_value="<mid@test>"):
        resp = await client.post(f"/accounts/{aid}/drafts/{draft_id}/send")
    assert resp.status_code == 200
    assert resp.json()["status"] == "sent"
    assert captured["kwargs"]["attachments"] is not None
    assert len(captured["kwargs"]["attachments"]) == 1
    assert captured["kwargs"]["attachments"][0]["filename"] == "report.pdf"
