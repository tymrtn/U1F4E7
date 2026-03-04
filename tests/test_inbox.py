# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import pytest
from unittest.mock import patch, MagicMock
import imaplib


async def _create_account(client):
    resp = await client.post("/accounts", json={
        "name": "Inbox Test",
        "host": "mail.example.com",
        "username": "test@example.com",
        "password": "secret",
    })
    return resp.json()


MOCK_SUMMARIES = [
    {
        "uid": "100",
        "message_id": "<msg100@example.com>",
        "from_addr": "alice@example.com",
        "to_addr": "test@example.com",
        "subject": "Hello",
        "date": "Mon, 20 Jan 2026 10:00:00 +0000",
        "flags": ["\\Seen"],
        "size": 1234,
    },
    {
        "uid": "99",
        "message_id": "<msg99@example.com>",
        "from_addr": "bob@example.com",
        "to_addr": "test@example.com",
        "subject": "Meeting",
        "date": "Sun, 19 Jan 2026 09:00:00 +0000",
        "flags": [],
        "size": 567,
    },
]

MOCK_MESSAGE = {
    "uid": "100",
    "message_id": "<msg100@example.com>",
    "from_addr": "alice@example.com",
    "to_addr": "test@example.com",
    "subject": "Hello",
    "date": "Mon, 20 Jan 2026 10:00:00 +0000",
    "in_reply_to": None,
    "references": None,
    "text_body": "Hi there",
    "html_body": "<p>Hi there</p>",
    "attachments": [
        {"filename": "doc.pdf", "content_type": "application/pdf", "size": 4096},
    ],
}


# --- List inbox ---


@pytest.mark.asyncio
async def test_list_inbox_default(client):
    account = await _create_account(client)

    with patch("app.main.search_messages") as mock_search:
        mock_search.return_value = MOCK_SUMMARIES
        resp = await client.get(f"/accounts/{account['id']}/inbox")
        assert resp.status_code == 200
        items = resp.json()
        assert len(items) == 2
        assert items[0]["uid"] == "100"
        mock_search.assert_called_once()
        call_kwargs = mock_search.call_args
        assert call_kwargs.kwargs["folder"] == "INBOX"
        assert call_kwargs.kwargs["query"] == "ALL"


@pytest.mark.asyncio
async def test_list_inbox_pagination(client):
    account = await _create_account(client)

    with patch("app.main.search_messages") as mock_search:
        mock_search.return_value = [MOCK_SUMMARIES[0]]
        resp = await client.get(f"/accounts/{account['id']}/inbox?limit=1&offset=0")
        assert resp.status_code == 200
        call_kwargs = mock_search.call_args
        assert call_kwargs.kwargs["limit"] == 1
        assert call_kwargs.kwargs["offset"] == 0


@pytest.mark.asyncio
async def test_list_inbox_search(client):
    account = await _create_account(client)

    with patch("app.main.search_messages") as mock_search:
        mock_search.return_value = [MOCK_SUMMARIES[0]]
        resp = await client.get(f"/accounts/{account['id']}/inbox?q=FROM alice@example.com")
        assert resp.status_code == 200
        call_kwargs = mock_search.call_args
        assert call_kwargs.kwargs["query"] == "FROM alice@example.com"


@pytest.mark.asyncio
async def test_list_inbox_folder(client):
    account = await _create_account(client)

    with patch("app.main.search_messages") as mock_search:
        mock_search.return_value = []
        resp = await client.get(f"/accounts/{account['id']}/inbox?folder=Sent")
        assert resp.status_code == 200
        call_kwargs = mock_search.call_args
        assert call_kwargs.kwargs["folder"] == "Sent"


# --- Get single message ---


@pytest.mark.asyncio
async def test_get_inbox_message(client):
    account = await _create_account(client)

    with patch("app.main.fetch_message") as mock_fetch:
        mock_fetch.return_value = MOCK_MESSAGE
        resp = await client.get(f"/accounts/{account['id']}/inbox/100")
        assert resp.status_code == 200
        msg = resp.json()
        assert msg["uid"] == "100"
        assert msg["text_body"] == "Hi there"
        assert len(msg["attachments"]) == 1
        assert msg["attachments"][0]["filename"] == "doc.pdf"


@pytest.mark.asyncio
async def test_get_inbox_message_not_found(client):
    account = await _create_account(client)

    with patch("app.main.fetch_message") as mock_fetch:
        mock_fetch.return_value = None
        resp = await client.get(f"/accounts/{account['id']}/inbox/999")
        assert resp.status_code == 404


@pytest.mark.asyncio
async def test_get_inbox_message_custom_folder(client):
    account = await _create_account(client)

    with patch("app.main.fetch_message") as mock_fetch:
        mock_fetch.return_value = MOCK_MESSAGE
        resp = await client.get(f"/accounts/{account['id']}/inbox/100?folder=Archive")
        assert resp.status_code == 200
        call_kwargs = mock_fetch.call_args
        assert call_kwargs.kwargs["folder"] == "Archive"


# --- Folders ---


@pytest.mark.asyncio
async def test_list_folders(client):
    account = await _create_account(client)

    with patch("app.main.list_folders") as mock_list:
        mock_list.return_value = ["INBOX", "Sent", "Drafts", "Trash", "Archive"]
        resp = await client.get(f"/accounts/{account['id']}/folders")
        assert resp.status_code == 200
        body = resp.json()
        assert "INBOX" in body["folders"]
        assert len(body["folders"]) == 5


# --- Error cases ---


@pytest.mark.asyncio
async def test_inbox_nonexistent_account(client):
    resp = await client.get("/accounts/nonexistent/inbox")
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_inbox_imap_error(client):
    from app.transport.imap import ImapError
    account = await _create_account(client)

    with patch("app.main.search_messages") as mock_search:
        mock_search.side_effect = ImapError("imap_error", "LOGIN failed")
        resp = await client.get(f"/accounts/{account['id']}/inbox")
        assert resp.status_code == 502
        body = resp.json()
        assert body["error_type"] == "imap_error"


@pytest.mark.asyncio
async def test_folders_nonexistent_account(client):
    resp = await client.get("/accounts/nonexistent/folders")
    assert resp.status_code == 404
