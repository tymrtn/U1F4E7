# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

"""Tests for IMAP operation API endpoints (move, flag, mark-read, delete, search)."""

import pytest
from unittest.mock import patch, AsyncMock
from app.transport.imap import ImapError


async def _create_account(client):
    resp = await client.post("/accounts", json={
        "name": "IMAP Ops Test",
        "host": "mail.example.com",
        "username": "test@example.com",
        "password": "secret",
    })
    return resp.json()


# --- POST /accounts/{account_id}/inbox/{uid}/move ---


@pytest.mark.asyncio
async def test_move_message(client):
    account = await _create_account(client)

    with patch("app.main.move_message", new_callable=AsyncMock) as mock_move:
        resp = await client.post(
            f"/accounts/{account['id']}/inbox/100/move",
            json={"folder": "Junk"},
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["status"] == "moved"
        assert body["uid"] == "100"
        assert body["folder"] == "Junk"
        mock_move.assert_called_once()
        call_kwargs = mock_move.call_args
        assert call_kwargs.kwargs["uid"] == "100"
        assert call_kwargs.kwargs["folder"] == "Junk"


@pytest.mark.asyncio
async def test_move_message_account_not_found(client):
    resp = await client.post(
        "/accounts/nonexistent/inbox/100/move",
        json={"folder": "Junk"},
    )
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_move_message_imap_error(client):
    account = await _create_account(client)

    with patch("app.main.move_message", new_callable=AsyncMock) as mock_move:
        mock_move.side_effect = ImapError("imap_error", "COPY failed")
        resp = await client.post(
            f"/accounts/{account['id']}/inbox/100/move",
            json={"folder": "Junk"},
        )
        assert resp.status_code == 502
        body = resp.json()
        assert body["error_type"] == "imap_error"


@pytest.mark.asyncio
async def test_move_message_missing_folder(client):
    account = await _create_account(client)
    resp = await client.post(
        f"/accounts/{account['id']}/inbox/100/move",
        json={},
    )
    assert resp.status_code == 422


# --- POST /accounts/{account_id}/inbox/{uid}/flag ---


@pytest.mark.asyncio
async def test_flag_message(client):
    account = await _create_account(client)

    with patch("app.main.set_flag", new_callable=AsyncMock) as mock_flag:
        resp = await client.post(
            f"/accounts/{account['id']}/inbox/100/flag",
            json={"flag": "\\Flagged", "folder": "INBOX"},
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["status"] == "flagged"
        assert body["uid"] == "100"
        assert body["flag"] == "\\Flagged"
        mock_flag.assert_called_once()
        call_kwargs = mock_flag.call_args
        assert call_kwargs.kwargs["uid"] == "100"
        assert call_kwargs.kwargs["flag"] == "\\Flagged"
        assert call_kwargs.kwargs["folder"] == "INBOX"


@pytest.mark.asyncio
async def test_flag_message_default_folder(client):
    account = await _create_account(client)

    with patch("app.main.set_flag", new_callable=AsyncMock) as mock_flag:
        resp = await client.post(
            f"/accounts/{account['id']}/inbox/100/flag",
            json={"flag": "\\Seen"},
        )
        assert resp.status_code == 200
        call_kwargs = mock_flag.call_args
        assert call_kwargs.kwargs["folder"] == "INBOX"


@pytest.mark.asyncio
async def test_flag_message_account_not_found(client):
    resp = await client.post(
        "/accounts/nonexistent/inbox/100/flag",
        json={"flag": "\\Flagged"},
    )
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_flag_message_imap_error(client):
    account = await _create_account(client)

    with patch("app.main.set_flag", new_callable=AsyncMock) as mock_flag:
        mock_flag.side_effect = ImapError("imap_error", "STORE failed")
        resp = await client.post(
            f"/accounts/{account['id']}/inbox/100/flag",
            json={"flag": "\\Flagged"},
        )
        assert resp.status_code == 502


# --- POST /accounts/{account_id}/inbox/{uid}/mark-read ---


@pytest.mark.asyncio
async def test_mark_read(client):
    account = await _create_account(client)

    with patch("app.main.mark_seen", new_callable=AsyncMock) as mock_seen:
        resp = await client.post(
            f"/accounts/{account['id']}/inbox/100/mark-read",
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["status"] == "marked_read"
        assert body["uid"] == "100"
        mock_seen.assert_called_once()
        call_kwargs = mock_seen.call_args
        assert call_kwargs.kwargs["uid"] == "100"
        assert call_kwargs.kwargs["folder"] == "INBOX"


@pytest.mark.asyncio
async def test_mark_read_custom_folder(client):
    account = await _create_account(client)

    with patch("app.main.mark_seen", new_callable=AsyncMock) as mock_seen:
        resp = await client.post(
            f"/accounts/{account['id']}/inbox/100/mark-read?folder=Archive",
        )
        assert resp.status_code == 200
        call_kwargs = mock_seen.call_args
        assert call_kwargs.kwargs["folder"] == "Archive"


@pytest.mark.asyncio
async def test_mark_read_account_not_found(client):
    resp = await client.post("/accounts/nonexistent/inbox/100/mark-read")
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_mark_read_imap_error(client):
    account = await _create_account(client)

    with patch("app.main.mark_seen", new_callable=AsyncMock) as mock_seen:
        mock_seen.side_effect = ImapError("connection_error", "Connection refused")
        resp = await client.post(
            f"/accounts/{account['id']}/inbox/100/mark-read",
        )
        assert resp.status_code == 502
        body = resp.json()
        assert body["error_type"] == "connection_error"


# --- DELETE /accounts/{account_id}/inbox/{uid} ---


@pytest.mark.asyncio
async def test_delete_message(client):
    account = await _create_account(client)

    with patch("app.main.delete_message", new_callable=AsyncMock) as mock_del:
        resp = await client.delete(
            f"/accounts/{account['id']}/inbox/100",
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["status"] == "deleted"
        assert body["uid"] == "100"
        mock_del.assert_called_once()
        call_kwargs = mock_del.call_args
        assert call_kwargs.kwargs["uid"] == "100"
        assert call_kwargs.kwargs["folder"] == "INBOX"


@pytest.mark.asyncio
async def test_delete_message_custom_folder(client):
    account = await _create_account(client)

    with patch("app.main.delete_message", new_callable=AsyncMock) as mock_del:
        resp = await client.delete(
            f"/accounts/{account['id']}/inbox/100?folder=Sent",
        )
        assert resp.status_code == 200
        call_kwargs = mock_del.call_args
        assert call_kwargs.kwargs["folder"] == "Sent"


@pytest.mark.asyncio
async def test_delete_message_account_not_found(client):
    resp = await client.delete("/accounts/nonexistent/inbox/100")
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_delete_message_imap_error(client):
    account = await _create_account(client)

    with patch("app.main.delete_message", new_callable=AsyncMock) as mock_del:
        mock_del.side_effect = ImapError("imap_error", "COPY to Trash failed")
        resp = await client.delete(
            f"/accounts/{account['id']}/inbox/100",
        )
        assert resp.status_code == 502


# --- GET /accounts/{account_id}/search ---


MOCK_SEARCH_RESULTS = [
    {
        "uid": "42",
        "message_id": "<msg42@example.com>",
        "from_addr": "alice@example.com",
        "to_addr": "test@example.com",
        "subject": "Important",
        "date": "Mon, 20 Jan 2026 10:00:00 +0000",
        "flags": [],
        "size": 1024,
    },
]


@pytest.mark.asyncio
async def test_search_messages(client):
    account = await _create_account(client)

    with patch("app.main.search_messages", new_callable=AsyncMock) as mock_search:
        mock_search.return_value = MOCK_SEARCH_RESULTS
        resp = await client.get(
            f"/accounts/{account['id']}/search?q=FROM alice@example.com",
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["query"] == "FROM alice@example.com"
        assert body["folder"] == "INBOX"
        assert body["count"] == 1
        assert len(body["results"]) == 1
        assert body["results"][0]["uid"] == "42"
        mock_search.assert_called_once()
        call_kwargs = mock_search.call_args
        assert call_kwargs.kwargs["query"] == "FROM alice@example.com"
        assert call_kwargs.kwargs["folder"] == "INBOX"
        assert call_kwargs.kwargs["limit"] == 50


@pytest.mark.asyncio
async def test_search_messages_custom_folder_and_limit(client):
    account = await _create_account(client)

    with patch("app.main.search_messages", new_callable=AsyncMock) as mock_search:
        mock_search.return_value = []
        resp = await client.get(
            f"/accounts/{account['id']}/search?q=UNSEEN&folder=Sent&limit=10",
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["folder"] == "Sent"
        assert body["count"] == 0
        call_kwargs = mock_search.call_args
        assert call_kwargs.kwargs["folder"] == "Sent"
        assert call_kwargs.kwargs["limit"] == 10


@pytest.mark.asyncio
async def test_search_messages_default_query(client):
    account = await _create_account(client)

    with patch("app.main.search_messages", new_callable=AsyncMock) as mock_search:
        mock_search.return_value = []
        resp = await client.get(f"/accounts/{account['id']}/search")
        assert resp.status_code == 200
        call_kwargs = mock_search.call_args
        assert call_kwargs.kwargs["query"] == "ALL"


@pytest.mark.asyncio
async def test_search_messages_account_not_found(client):
    resp = await client.get("/accounts/nonexistent/search?q=ALL")
    assert resp.status_code == 404


@pytest.mark.asyncio
async def test_search_messages_imap_error(client):
    account = await _create_account(client)

    with patch("app.main.search_messages", new_callable=AsyncMock) as mock_search:
        mock_search.side_effect = ImapError("imap_error", "SEARCH failed")
        resp = await client.get(
            f"/accounts/{account['id']}/search?q=ALL",
        )
        assert resp.status_code == 502
        body = resp.json()
        assert body["error_type"] == "imap_error"
