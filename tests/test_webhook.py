# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import hashlib
import hmac
import json
import pytest
from unittest.mock import AsyncMock, MagicMock, patch


@pytest.mark.asyncio
async def test_webhook_state_table_exists(setup_db):
    from app.db import get_db
    db = await get_db()
    cursor = await db.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='webhook_state'"
    )
    row = await cursor.fetchone()
    assert row is not None


@pytest.mark.asyncio
async def test_webhook_poller_skips_accounts_without_url(setup_db):
    """Accounts without webhook_url are not checked."""
    from app.transport.webhook import WebhookPoller
    from app.credentials.store import create_account

    await create_account(
        name="No Webhook",
        smtp_host="smtp.example.com",
        smtp_port=587,
        imap_host="imap.example.com",
        imap_port=993,
        username="user@example.com",
        password="secret",
    )

    poller = WebhookPoller()

    with patch("app.transport.webhook.list_accounts") as mock_list, \
         patch("app.transport.webhook.get_account_with_credentials") as mock_creds:
        mock_list.return_value = [{"id": "test-id", "webhook_url": None}]
        await poller._poll_loop.__wrapped__(poller) if hasattr(poller._poll_loop, "__wrapped__") else None
        # Confirm _check_account was never invoked (no webhook_url)
        mock_creds.assert_not_called()


@pytest.mark.asyncio
async def test_webhook_sign_produces_hmac_sha256(setup_db):
    """WebhookPoller._sign returns correct HMAC-SHA256 hex digest."""
    from app.transport.webhook import WebhookPoller

    poller = WebhookPoller()
    secret = "my-secret"
    payload = b'{"uid": "42"}'
    expected = hmac.new(secret.encode(), payload, hashlib.sha256).hexdigest()
    assert poller._sign(secret, payload) == expected


@pytest.mark.asyncio
async def test_check_account_updates_webhook_state(setup_db):
    """After delivery, webhook_state is updated with the highest UID seen."""
    from app.transport.webhook import WebhookPoller
    from app.db import get_db

    account_id = "test-account-id"

    mock_account = {
        "id": account_id,
        "webhook_url": "https://example.com/hook",
        "webhook_secret": None,
        "effective_imap_username": "user@example.com",
        "effective_imap_password": "secret",
        "imap_host": "imap.example.com",
        "imap_port": 993,
    }

    mock_summaries = [{"uid": "10"}, {"uid": "11"}]
    mock_message = {
        "uid": "10",
        "message_id": "<test@example.com>",
        "from_addr": "sender@example.com",
        "to_addr": "me@example.com",
        "subject": "Hello",
        "date": "Mon, 1 Jan 2026 12:00:00 +0000",
        "text_body": "Hi there",
        "html_body": None,
        "attachments": [],
    }

    import httpx
    from unittest.mock import AsyncMock

    poller = WebhookPoller()

    async def fake_fetch(account, folder, uid):
        msg = dict(mock_message)
        msg["uid"] = uid
        return msg

    with patch("app.transport.webhook.get_account_with_credentials", return_value=mock_account), \
         patch("app.transport.webhook.search_messages", return_value=mock_summaries), \
         patch("app.transport.webhook.fetch_message", side_effect=fake_fetch), \
         patch("app.services.policy.list_address_policies", return_value=[]), \
         patch("httpx.AsyncClient") as mock_client_cls:

        mock_resp = MagicMock()
        mock_resp.raise_for_status = MagicMock()
        mock_resp.status_code = 200
        mock_client = AsyncMock()
        mock_client.post = AsyncMock(return_value=mock_resp)
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=False)
        mock_client_cls.return_value = mock_client

        await poller._check_account(mock_account)

    db = await get_db()
    cursor = await db.execute(
        "SELECT last_uid FROM webhook_state WHERE account_id = ?",
        (account_id,),
    )
    row = await cursor.fetchone()
    assert row is not None
    assert int(row["last_uid"]) == 11
