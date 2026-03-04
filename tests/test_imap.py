import pytest
from unittest.mock import patch, MagicMock, AsyncMock
from app.transport.imap import (
    fetch_unread,
    mark_seen,
    get_thread,
    InboundMessage,
    _decode_header_value,
    _extract_body,
    _fetch_unread_sync,
    _search_thread_sync,
    _parse_message_ids,
)
import email


MOCK_ACCOUNT = {
    "imap_host": "imap.example.com",
    "imap_port": 993,
    "effective_imap_username": "user@example.com",
    "effective_imap_password": "secret",
}


class TestDecodeHeader:
    def test_plain_ascii(self):
        assert _decode_header_value("Hello World") == "Hello World"

    def test_empty_string(self):
        assert _decode_header_value("") == ""

    def test_none(self):
        assert _decode_header_value(None) == ""


class TestExtractBody:
    def test_plain_text_message(self):
        msg = email.message.EmailMessage()
        msg.set_content("Hello plain text")
        text, html = _extract_body(msg)
        assert "Hello plain text" in text
        assert html is None

    def test_html_message(self):
        msg = email.message.EmailMessage()
        msg.set_content("<p>Hello HTML</p>", subtype="html")
        text, html = _extract_body(msg)
        assert html is not None
        assert "Hello HTML" in html

    def test_multipart_message(self):
        msg = email.message.EmailMessage()
        msg.set_content("Plain text version")
        msg.add_alternative("<p>HTML version</p>", subtype="html")
        text, html = _extract_body(msg)
        assert "Plain text" in text
        assert "HTML version" in html


class TestFetchUnread:
    @pytest.mark.asyncio
    async def test_fetch_unread_calls_sync(self):
        with patch("app.transport.imap._fetch_unread_sync", return_value=[]) as mock_sync:
            result = await fetch_unread(MOCK_ACCOUNT)
            assert result == []
            mock_sync.assert_called_once_with(
                host="imap.example.com",
                port=993,
                username="user@example.com",
                password="secret",
                folder="INBOX",
            )

    @pytest.mark.asyncio
    async def test_fetch_unread_custom_folder(self):
        with patch("app.transport.imap._fetch_unread_sync", return_value=[]) as mock_sync:
            await fetch_unread(MOCK_ACCOUNT, folder="Sent")
            mock_sync.assert_called_once_with(
                host="imap.example.com",
                port=993,
                username="user@example.com",
                password="secret",
                folder="Sent",
            )


class TestMarkSeen:
    @pytest.mark.asyncio
    async def test_mark_seen_calls_sync(self):
        with patch("app.transport.imap._mark_seen_sync") as mock_sync:
            await mark_seen(MOCK_ACCOUNT, "123")
            mock_sync.assert_called_once_with(
                host="imap.example.com",
                port=993,
                username="user@example.com",
                password="secret",
                uid="123",
                folder="INBOX",
            )


class TestFetchUnreadSync:
    def test_no_unseen_messages(self):
        mock_conn = MagicMock()
        mock_conn.uid.side_effect = [
            ("OK", [b""]),  # SEARCH returns empty
        ]

        with patch("app.transport.imap.imaplib.IMAP4_SSL", return_value=mock_conn):
            result = _fetch_unread_sync(
                "imap.example.com", 993, "user", "pass"
            )
            assert result == []
            mock_conn.login.assert_called_once_with("user", "pass")
            mock_conn.select.assert_called_once_with("INBOX")

    def test_parses_single_message(self):
        raw_email = (
            b"From: sender@example.com\r\n"
            b"To: info@loftly.com\r\n"
            b"Subject: Test\r\n"
            b"Message-ID: <abc@example.com>\r\n"
            b"\r\n"
            b"Hello world"
        )
        mock_conn = MagicMock()
        mock_conn.uid.side_effect = [
            ("OK", [b"1"]),  # SEARCH
            ("OK", [(b"1 (RFC822 {100}", raw_email), b")"]),  # FETCH
        ]

        with patch("app.transport.imap.imaplib.IMAP4_SSL", return_value=mock_conn):
            result = _fetch_unread_sync(
                "imap.example.com", 993, "user", "pass"
            )
            assert len(result) == 1
            assert result[0].uid == "1"
            assert result[0].subject == "Test"
            assert result[0].from_addr == "sender@example.com"
            assert "Hello world" in result[0].text_body
            assert result[0].message_id == "<abc@example.com>"


class TestParseMessageIds:
    def test_single_id(self):
        assert _parse_message_ids("<abc@example.com>") == ["<abc@example.com>"]

    def test_multiple_ids(self):
        result = _parse_message_ids("<a@x.com> <b@x.com> <c@x.com>")
        assert len(result) == 3

    def test_empty(self):
        assert _parse_message_ids("") == []
        assert _parse_message_ids(None) == []


class TestGetThread:
    @pytest.mark.asyncio
    async def test_get_thread_calls_sync(self):
        with patch("app.transport.imap._search_thread_sync", return_value=[]) as mock_sync:
            result = await get_thread(MOCK_ACCOUNT, "<msg@test.com>")
            assert result == []
            mock_sync.assert_called_once_with(
                host="imap.example.com",
                port=993,
                username="user@example.com",
                password="secret",
                message_id="<msg@test.com>",
                folder="INBOX",
            )

    @pytest.mark.asyncio
    async def test_get_thread_custom_folder(self):
        with patch("app.transport.imap._search_thread_sync", return_value=[]) as mock_sync:
            await get_thread(MOCK_ACCOUNT, "<msg@test.com>", folder="Sent")
            mock_sync.assert_called_once_with(
                host="imap.example.com",
                port=993,
                username="user@example.com",
                password="secret",
                message_id="<msg@test.com>",
                folder="Sent",
            )
