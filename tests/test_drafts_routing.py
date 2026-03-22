# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import pytest
from unittest.mock import patch, AsyncMock

from app.services.compose import HUMAN_APPROVAL_SOURCES, send_draft, DraftRoutingError


def test_cli_not_in_human_approval_sources():
    """cli must not be a trusted approval source."""
    assert "cli" not in HUMAN_APPROVAL_SOURCES


@pytest.mark.asyncio
async def test_send_draft_rejects_pending_review():
    """send_draft refuses to send a draft with status pending_review."""
    mock_draft = {
        "id": "d1", "account_id": "a1", "status": "pending_review",
        "to_addr": "to@test.com", "subject": "Test", "text_content": "Hi",
        "html_content": None, "in_reply_to": None, "cc_addr": None,
        "bcc_addr": None, "reply_to": None, "attachments": None,
        "metadata": {},
    }
    with patch("app.services.compose.drafts.get_draft", new_callable=AsyncMock, return_value=mock_draft):
        with pytest.raises(DraftRoutingError) as exc_info:
            await send_draft("a1", "d1")
        assert exc_info.value.status_code == 403


@pytest.mark.asyncio
async def test_send_draft_rejects_blocked():
    """send_draft refuses to send a blocked draft."""
    mock_draft = {
        "id": "d1", "account_id": "a1", "status": "blocked",
        "to_addr": "to@test.com", "subject": "Test", "text_content": "Hi",
        "html_content": None, "in_reply_to": None, "cc_addr": None,
        "bcc_addr": None, "reply_to": None, "attachments": None,
        "metadata": {},
    }
    with patch("app.services.compose.drafts.get_draft", new_callable=AsyncMock, return_value=mock_draft):
        with pytest.raises(DraftRoutingError) as exc_info:
            await send_draft("a1", "d1")
        assert exc_info.value.status_code == 403


@pytest.mark.asyncio
async def test_send_draft_allows_draft_status():
    """send_draft still works for drafts in 'draft' status (auto-send/delayed zone)."""
    mock_draft = {
        "id": "d1", "account_id": "a1", "status": "draft",
        "to_addr": "to@test.com", "subject": "Test", "text_content": "Hi",
        "html_content": None, "in_reply_to": None, "cc_addr": None,
        "bcc_addr": None, "reply_to": None, "attachments": None,
        "metadata": {},
    }
    mock_account = {
        "id": "a1", "username": "user@test.com", "display_name": None,
        "signature_text": None, "signature_html": None,
        "effective_smtp_username": "user@test.com",
        "effective_smtp_password": "pass",
    }
    with patch("app.services.compose.drafts.get_draft", new_callable=AsyncMock, return_value=mock_draft), \
         patch("app.services.compose.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=mock_account), \
         patch("app.services.compose.send_message", new_callable=AsyncMock, return_value="<mid@test>"), \
         patch("app.services.compose.messages.create_message", new_callable=AsyncMock, return_value={"id": "m1"}), \
         patch("app.services.compose.messages.mark_sent", new_callable=AsyncMock), \
         patch("app.services.compose.drafts.mark_draft_sent", new_callable=AsyncMock):
        result = await send_draft("a1", "d1")
        assert result["status"] == "sent"


@pytest.mark.asyncio
async def test_route_pending_review_saves_to_imap_drafts():
    """When routing scores pending_review, the MIME message is saved to IMAP Drafts."""
    mock_account = {
        "id": "a1", "username": "user@test.com", "domain": "test.com",
        "display_name": None, "signature_text": None, "signature_html": None,
        "auto_send_threshold": 0.85, "review_threshold": 0.50,
        "delay_margin": 0.10, "delay_minutes": 15,
    }
    mock_account_creds = {**mock_account,
        "effective_imap_username": "user@test.com",
        "effective_imap_password": "pass",
        "effective_smtp_username": "user@test.com",
        "effective_smtp_password": "pass",
        "encrypted_password": "x",
    }
    mock_draft = {
        "id": "d1", "account_id": "a1", "status": "pending_review",
        "to_addr": "to@test.com", "subject": "Test", "text_content": "Hi",
        "html_content": None, "in_reply_to": None, "metadata": {},
    }

    with patch("app.services.compose.credential_store.get_account", new_callable=AsyncMock, return_value=mock_account), \
         patch("app.services.compose.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=mock_account_creds), \
         patch("app.services.compose.scoring_svc.get_attribute_catalog", new_callable=AsyncMock, return_value=("test.com", 0.80, [])), \
         patch("app.services.compose.scoring_svc.compute_attribute_score", return_value=(0.55, {"reply": 0.10})), \
         patch("app.services.compose.drafts.create_draft", new_callable=AsyncMock, return_value=mock_draft), \
         patch("app.services.compose.log_action", new_callable=AsyncMock), \
         patch("app.services.compose.save_to_drafts", new_callable=AsyncMock) as mock_save, \
         patch("os.getenv", return_value=""):
        from app.services.compose import route_composed_email
        result = await route_composed_email(
            account_id="a1",
            to_addr="to@test.com",
            justification="test",
            attribution={"attributes": ["reply"]},
            subject="Test",
            text_content="Hi",
        )
        assert result["routing_status"] == "pending_review"
        mock_save.assert_called_once()
        saved_account = mock_save.call_args[0][0]
        assert "effective_imap_username" in saved_account


@pytest.mark.asyncio
async def test_route_auto_sent_does_not_save_to_imap_drafts():
    """When routing scores 'sent' (auto-send), save_to_drafts must NOT be called."""
    mock_account = {
        "id": "a1", "username": "user@test.com", "domain": "test.com",
        "display_name": None, "signature_text": None, "signature_html": None,
        "auto_send_threshold": 0.85, "review_threshold": 0.50,
        "delay_margin": 0.10, "delay_minutes": 15,
    }
    mock_account_creds = {**mock_account,
        "effective_imap_username": "user@test.com",
        "effective_imap_password": "pass",
        "effective_smtp_username": "user@test.com",
        "effective_smtp_password": "pass",
        "encrypted_password": "x",
    }
    mock_draft = {
        "id": "d1", "account_id": "a1", "status": "draft",
        "to_addr": "to@test.com", "subject": "Test", "text_content": "Hi",
        "html_content": None, "in_reply_to": None, "metadata": {},
        "cc_addr": None, "bcc_addr": None, "reply_to": None,
        "attachments": None,
    }

    with patch("app.services.compose.credential_store.get_account", new_callable=AsyncMock, return_value=mock_account), \
         patch("app.services.compose.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=mock_account_creds), \
         patch("app.services.compose.scoring_svc.get_attribute_catalog", new_callable=AsyncMock, return_value=("test.com", 0.80, [])), \
         patch("app.services.compose.scoring_svc.compute_attribute_score", return_value=(0.95, {"reply": 0.15})), \
         patch("app.services.compose.drafts.create_draft", new_callable=AsyncMock, return_value=mock_draft), \
         patch("app.services.compose.drafts.get_draft", new_callable=AsyncMock, return_value=mock_draft), \
         patch("app.services.compose.log_action", new_callable=AsyncMock), \
         patch("app.services.compose.save_to_drafts", new_callable=AsyncMock) as mock_save, \
         patch("app.services.compose.send_message", new_callable=AsyncMock, return_value="<mid@test>"), \
         patch("app.services.compose.messages.create_message", new_callable=AsyncMock, return_value={"id": "m1"}), \
         patch("app.services.compose.messages.mark_sent", new_callable=AsyncMock), \
         patch("app.services.compose.drafts.mark_draft_sent", new_callable=AsyncMock), \
         patch("os.getenv", return_value=""):
        from app.services.compose import route_composed_email
        result = await route_composed_email(
            account_id="a1",
            to_addr="to@test.com",
            justification="test",
            attribution={"attributes": ["reply"]},
            subject="Test",
            text_content="Hi",
        )
        assert result["routing_status"] == "sent"
        mock_save.assert_not_called()


@pytest.mark.asyncio
async def test_send_draft_sets_references_header_on_reply():
    """send_draft must set both In-Reply-To and References headers for threading."""
    mock_draft = {
        "id": "d1", "account_id": "a1", "status": "draft",
        "to_addr": "to@test.com", "subject": "Re: Test", "text_content": "Hi",
        "html_content": None, "in_reply_to": "<orig@test.com>", "cc_addr": None,
        "bcc_addr": None, "reply_to": None, "attachments": None,
        "metadata": {},
    }
    mock_account = {
        "id": "a1", "username": "user@test.com", "display_name": None,
        "signature_text": None, "signature_html": None,
        "effective_smtp_username": "user@test.com",
        "effective_smtp_password": "pass",
    }
    captured_msg = {}

    async def capture_send(account, msg, pool=None):
        captured_msg["In-Reply-To"] = msg["In-Reply-To"]
        captured_msg["References"] = msg["References"]
        return "<mid@test>"

    with patch("app.services.compose.drafts.get_draft", new_callable=AsyncMock, return_value=mock_draft), \
         patch("app.services.compose.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=mock_account), \
         patch("app.services.compose.send_message", side_effect=capture_send), \
         patch("app.services.compose.messages.create_message", new_callable=AsyncMock, return_value={"id": "m1"}), \
         patch("app.services.compose.messages.mark_sent", new_callable=AsyncMock), \
         patch("app.services.compose.drafts.mark_draft_sent", new_callable=AsyncMock):
        await send_draft("a1", "d1")
        assert captured_msg["In-Reply-To"] == "<orig@test.com>"
        assert captured_msg["References"] == "<orig@test.com>"


@pytest.mark.asyncio
async def test_route_blocked_reply_sets_references_on_imap_draft():
    """When a reply is routed to blocked, the IMAP draft has both In-Reply-To and References."""
    mock_account = {
        "id": "a1", "username": "user@test.com", "domain": "test.com",
        "display_name": None, "signature_text": None, "signature_html": None,
        "auto_send_threshold": 0.85, "review_threshold": 0.50,
        "delay_margin": 0.10, "delay_minutes": 15,
    }
    mock_account_creds = {**mock_account,
        "effective_imap_username": "user@test.com",
        "effective_imap_password": "pass",
        "effective_smtp_username": "user@test.com",
        "effective_smtp_password": "pass",
        "encrypted_password": "x",
    }
    mock_draft = {
        "id": "d1", "account_id": "a1", "status": "blocked",
        "to_addr": "to@test.com", "subject": "Re: Test", "text_content": "Hi",
        "html_content": None, "in_reply_to": "<orig@test.com>", "metadata": {},
    }

    with patch("app.services.compose.credential_store.get_account", new_callable=AsyncMock, return_value=mock_account), \
         patch("app.services.compose.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=mock_account_creds), \
         patch("app.services.compose.scoring_svc.get_attribute_catalog", new_callable=AsyncMock, return_value=("test.com", 0.80, [])), \
         patch("app.services.compose.scoring_svc.compute_attribute_score", return_value=(0.30, {})), \
         patch("app.services.compose.drafts.create_draft", new_callable=AsyncMock, return_value=mock_draft), \
         patch("app.services.compose.log_action", new_callable=AsyncMock), \
         patch("app.services.compose.save_to_drafts", new_callable=AsyncMock) as mock_save, \
         patch("os.getenv", return_value=""):
        from app.services.compose import route_composed_email
        result = await route_composed_email(
            account_id="a1",
            to_addr="to@test.com",
            justification="test",
            attribution={"attributes": []},
            subject="Re: Test",
            text_content="Hi",
            in_reply_to="<orig@test.com>",
        )
        assert result["routing_status"] == "blocked"
        mock_save.assert_called_once()
        saved_mime = mock_save.call_args[0][1]
        assert saved_mime["In-Reply-To"] == "<orig@test.com>"
        assert saved_mime["References"] == "<orig@test.com>"
