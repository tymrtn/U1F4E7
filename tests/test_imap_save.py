# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import pytest
from unittest.mock import patch, MagicMock
from email.message import EmailMessage

from app.transport.imap import save_to_drafts, _detect_drafts_folder, _drafts_folder_cache


ACCOUNT = {
    "imap_host": "imap.example.com",
    "imap_port": 993,
    "effective_imap_username": "user@example.com",
    "effective_imap_password": "pass",
}

GMAIL_ACCOUNT = {
    "imap_host": "imap.gmail.com",
    "imap_port": 993,
    "effective_imap_username": "user@gmail.com",
    "effective_imap_password": "pass",
}


def _make_msg():
    msg = EmailMessage()
    msg["From"] = "user@example.com"
    msg["To"] = "recipient@example.com"
    msg["Subject"] = "Test draft"
    msg.set_content("Hello")
    return msg


def _mock_conn_with_folders(folder_lines: list[bytes]):
    mock_conn = MagicMock()
    mock_conn.append.return_value = ("OK", [b"1"])
    mock_conn.list.return_value = ("OK", folder_lines)
    return mock_conn


@pytest.fixture(autouse=True)
def clear_drafts_cache():
    _drafts_folder_cache.clear()
    yield
    _drafts_folder_cache.clear()


@pytest.mark.asyncio
async def test_save_to_drafts_appends_with_draft_flag():
    """save_to_drafts APPENDs message to Drafts folder with \\Draft flag."""
    mock_conn = _mock_conn_with_folders([
        b'(\\HasNoChildren \\Drafts) "/" "Drafts"',
    ])

    with patch("app.transport.imap.imaplib.IMAP4_SSL", return_value=mock_conn):
        await save_to_drafts(ACCOUNT, _make_msg())

    mock_conn.login.assert_called_once_with("user@example.com", "pass")
    mock_conn.append.assert_called_once()
    call_args = mock_conn.append.call_args
    assert call_args[0][0] == "Drafts"
    assert "\\Draft" in call_args[0][1]
    mock_conn.logout.assert_called_once()


@pytest.mark.asyncio
async def test_save_to_drafts_custom_folder():
    """save_to_drafts respects custom folder name without auto-detection."""
    mock_conn = _mock_conn_with_folders([])

    with patch("app.transport.imap.imaplib.IMAP4_SSL", return_value=mock_conn):
        await save_to_drafts(ACCOUNT, _make_msg(), folder="INBOX.Drafts")

    # LIST should NOT be called when a custom folder is provided
    mock_conn.list.assert_not_called()
    call_args = mock_conn.append.call_args
    assert call_args[0][0] == "INBOX.Drafts"


@pytest.mark.asyncio
async def test_save_to_drafts_detects_gmail_drafts():
    """Gmail's [Gmail]/Drafts is detected and used."""
    mock_conn = _mock_conn_with_folders([
        b'(\\HasNoChildren) "/" "INBOX"',
        b'(\\HasNoChildren \\Sent) "/" "[Gmail]/Sent Mail"',
        b'(\\HasNoChildren \\Drafts) "/" "[Gmail]/Drafts"',
        b'(\\HasNoChildren \\Trash) "/" "[Gmail]/Trash"',
    ])

    with patch("app.transport.imap.imaplib.IMAP4_SSL", return_value=mock_conn):
        await save_to_drafts(GMAIL_ACCOUNT, _make_msg())

    call_args = mock_conn.append.call_args
    assert call_args[0][0] == "[Gmail]/Drafts"


@pytest.mark.asyncio
async def test_save_to_drafts_detects_inbox_dot_drafts():
    """Dovecot-style INBOX.Drafts is detected."""
    mock_conn = _mock_conn_with_folders([
        b'(\\HasNoChildren) "." "INBOX"',
        b'(\\HasNoChildren \\Drafts) "." "INBOX.Drafts"',
        b'(\\HasNoChildren \\Sent) "." "INBOX.Sent"',
    ])

    with patch("app.transport.imap.imaplib.IMAP4_SSL", return_value=mock_conn):
        await save_to_drafts(ACCOUNT, _make_msg())

    call_args = mock_conn.append.call_args
    assert call_args[0][0] == "INBOX.Drafts"


@pytest.mark.asyncio
async def test_save_to_drafts_caches_detected_folder():
    """Detected drafts folder is cached per (host, username)."""
    mock_conn = _mock_conn_with_folders([
        b'(\\HasNoChildren \\Drafts) "/" "[Gmail]/Drafts"',
    ])

    with patch("app.transport.imap.imaplib.IMAP4_SSL", return_value=mock_conn):
        await save_to_drafts(GMAIL_ACCOUNT, _make_msg())
        # Second call should use cache
        await save_to_drafts(GMAIL_ACCOUNT, _make_msg())

    # LIST should only be called once (first call), not on the second
    assert mock_conn.list.call_count == 1
    assert _drafts_folder_cache[("imap.gmail.com", "user@gmail.com")] == "[Gmail]/Drafts"


@pytest.mark.asyncio
async def test_save_to_drafts_fallback_when_no_drafts_folder():
    """Falls back to 'Drafts' when no drafts folder is found in LIST."""
    mock_conn = _mock_conn_with_folders([
        b'(\\HasNoChildren) "/" "INBOX"',
        b'(\\HasNoChildren) "/" "Sent"',
    ])

    with patch("app.transport.imap.imaplib.IMAP4_SSL", return_value=mock_conn):
        await save_to_drafts(ACCOUNT, _make_msg())

    call_args = mock_conn.append.call_args
    assert call_args[0][0] == "Drafts"


def test_detect_drafts_folder_prefers_exact_drafts():
    """When both 'Drafts' and '[Gmail]/Drafts' exist, prefer exact 'Drafts'."""
    mock_conn = MagicMock()
    mock_conn.list.return_value = ("OK", [
        b'(\\HasNoChildren) "/" "Drafts"',
        b'(\\HasNoChildren) "/" "[Gmail]/Drafts"',
    ])

    result = _detect_drafts_folder(mock_conn)
    assert result == "Drafts"
