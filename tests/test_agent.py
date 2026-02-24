import json
import os
import pytest
from unittest.mock import patch, AsyncMock, MagicMock

from app.agent.inbox_agent import InboxAgent
from app.agent.llm import LLMResponse
from app.transport.imap import InboundMessage


def _make_inbound(
    uid="1",
    message_id="<test@example.com>",
    from_addr="visitor@example.com",
    subject="Test Subject",
    text_body="What is the price?",
) -> InboundMessage:
    return InboundMessage(
        uid=uid,
        message_id=message_id,
        from_addr=from_addr,
        to_addr="info@loftly.com",
        subject=subject,
        text_body=text_body,
        html_body=None,
        in_reply_to=None,
        references=None,
        date="Mon, 1 Jan 2026 12:00:00 +0000",
    )


def _make_llm_response(classification="auto_reply", confidence=0.9, draft="Thanks for asking!"):
    return LLMResponse(
        content=json.dumps({
            "classification": classification,
            "confidence": confidence,
            "reasoning": "Test reasoning",
            "draft_reply": draft,
            "escalation_note": None if classification != "escalate" else "Need human input",
        }),
        model="test-model",
        usage={},
    )


MOCK_ACCOUNT = {
    "id": "acc-123",
    "username": "info@loftly.com",
    "imap_host": "imap.migadu.com",
    "imap_port": 993,
    "smtp_host": "smtp.migadu.com",
    "smtp_port": 587,
    "effective_imap_username": "info@loftly.com",
    "effective_imap_password": "secret",
    "effective_smtp_username": "info@loftly.com",
    "effective_smtp_password": "secret",
    "display_name": "Loftly",
}


class TestInboxAgentStatus:
    def test_initial_status(self):
        agent = InboxAgent(MagicMock())
        status = agent.status()
        assert status["running"] is False
        assert status["last_poll"] is None
        assert status["poll_count"] == 0

    def test_config_from_env(self):
        with patch.dict(os.environ, {
            "AGENT_ACCOUNT_ID": "test-id",
            "AGENT_POLL_INTERVAL": "60",
            "AGENT_ESCALATION_EMAIL": "tyler@loftly.com",
            "AGENT_SEND_FROM": "hello@loftly.com",
        }):
            agent = InboxAgent(MagicMock())
            assert agent.account_id == "test-id"
            assert agent.poll_interval == 60
            assert agent.escalation_email == "tyler@loftly.com"
            assert agent.send_from == "hello@loftly.com"

    def test_config_override(self):
        agent = InboxAgent(MagicMock(), config={
            "account_id": "override-id",
            "poll_interval": 30,
        })
        assert agent.account_id == "override-id"
        assert agent.poll_interval == 30


class TestInboxAgentParseLLM:
    def test_valid_json(self):
        agent = InboxAgent(MagicMock())
        result = agent._parse_llm_response('{"classification": "ignore", "confidence": 1.0}')
        assert result["classification"] == "ignore"

    def test_json_in_code_block(self):
        agent = InboxAgent(MagicMock())
        content = '```json\n{"classification": "auto_reply", "confidence": 0.9}\n```'
        result = agent._parse_llm_response(content)
        assert result["classification"] == "auto_reply"

    def test_invalid_json_escalates(self):
        agent = InboxAgent(MagicMock())
        result = agent._parse_llm_response("This is not JSON at all")
        assert result["classification"] == "escalate"
        assert result["confidence"] == 0.0


class TestInboxAgentExtractEmail:
    def test_with_display_name(self):
        assert InboxAgent._extract_email("John <john@example.com>") == "john@example.com"

    def test_plain_email(self):
        assert InboxAgent._extract_email("john@example.com") == "john@example.com"


class TestInboxAgentPoll:
    @pytest.mark.asyncio
    async def test_poll_no_account_id(self):
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop("AGENT_ACCOUNT_ID", None)
            agent = InboxAgent(MagicMock())
            results = await agent.poll_once()
            assert results == []

    @pytest.mark.asyncio
    async def test_poll_account_not_found(self):
        with (
            patch.dict(os.environ, {"AGENT_ACCOUNT_ID": "missing-id"}),
            patch(
                "app.agent.inbox_agent.credential_store.get_account_with_credentials",
                new_callable=AsyncMock,
                return_value=None,
            ),
        ):
            agent = InboxAgent(MagicMock())
            results = await agent.poll_once()
            assert results == []

    @pytest.mark.asyncio
    async def test_poll_no_unread(self):
        with (
            patch.dict(os.environ, {"AGENT_ACCOUNT_ID": "acc-123"}),
            patch(
                "app.agent.inbox_agent.credential_store.get_account_with_credentials",
                new_callable=AsyncMock,
                return_value=MOCK_ACCOUNT,
            ),
            patch(
                "app.agent.inbox_agent.fetch_unread",
                new_callable=AsyncMock,
                return_value=[],
            ),
        ):
            agent = InboxAgent(MagicMock())
            results = await agent.poll_once()
            assert results == []
            assert agent._poll_count == 1
            assert agent._last_poll is not None

    @pytest.mark.asyncio
    async def test_poll_auto_reply(self):
        inbound = _make_inbound()
        llm_resp = _make_llm_response("auto_reply", 0.92, "The price starts at EUR 125k.")

        with (
            patch.dict(os.environ, {
                "AGENT_ACCOUNT_ID": "acc-123",
                "AGENT_SEND_FROM": "hello@loftly.com",
            }),
            patch(
                "app.agent.inbox_agent.credential_store.get_account_with_credentials",
                new_callable=AsyncMock,
                return_value=MOCK_ACCOUNT,
            ),
            patch(
                "app.agent.inbox_agent.fetch_unread",
                new_callable=AsyncMock,
                return_value=[inbound],
            ),
            patch(
                "app.agent.inbox_agent.chat_completion",
                new_callable=AsyncMock,
                return_value=llm_resp,
            ),
            patch(
                "app.agent.inbox_agent.send_message",
                new_callable=AsyncMock,
                return_value="<reply-id@loftly.com>",
            ) as mock_send,
            patch(
                "app.agent.inbox_agent.mark_seen",
                new_callable=AsyncMock,
            ) as mock_seen,
            patch("app.agent.inbox_agent.get_db", new_callable=AsyncMock) as mock_db,
        ):
            db_instance = AsyncMock()
            mock_db.return_value = db_instance
            db_instance.execute.return_value = AsyncMock(fetchone=AsyncMock(return_value=None))

            agent = InboxAgent(MagicMock())
            results = await agent.poll_once()

            assert len(results) == 1
            assert results[0]["action"] == "auto_reply"
            mock_send.assert_called_once()
            mock_seen.assert_called_once()

    @pytest.mark.asyncio
    async def test_poll_ignore(self):
        inbound = _make_inbound(subject="Your weekly newsletter", text_body="Unsubscribe here")
        llm_resp = _make_llm_response("ignore", 0.98, None)

        with (
            patch.dict(os.environ, {"AGENT_ACCOUNT_ID": "acc-123"}),
            patch(
                "app.agent.inbox_agent.credential_store.get_account_with_credentials",
                new_callable=AsyncMock,
                return_value=MOCK_ACCOUNT,
            ),
            patch(
                "app.agent.inbox_agent.fetch_unread",
                new_callable=AsyncMock,
                return_value=[inbound],
            ),
            patch(
                "app.agent.inbox_agent.chat_completion",
                new_callable=AsyncMock,
                return_value=llm_resp,
            ),
            patch(
                "app.agent.inbox_agent.mark_seen",
                new_callable=AsyncMock,
            ) as mock_seen,
            patch("app.agent.inbox_agent.get_db", new_callable=AsyncMock) as mock_db,
        ):
            db_instance = AsyncMock()
            mock_db.return_value = db_instance
            db_instance.execute.return_value = AsyncMock(fetchone=AsyncMock(return_value=None))

            agent = InboxAgent(MagicMock())
            results = await agent.poll_once()

            assert len(results) == 1
            assert results[0]["action"] == "ignore"
            mock_seen.assert_called_once()

    @pytest.mark.asyncio
    async def test_poll_draft_for_review_creates_draft(self):
        inbound = _make_inbound(subject="Pricing question", text_body="How much for Denia?")
        llm_resp = _make_llm_response("draft_for_review", 0.75, "The price starts around EUR 125k.")

        with (
            patch.dict(os.environ, {
                "AGENT_ACCOUNT_ID": "acc-123",
                "AGENT_SEND_FROM": "hello@loftly.com",
            }),
            patch(
                "app.agent.inbox_agent.credential_store.get_account_with_credentials",
                new_callable=AsyncMock,
                return_value=MOCK_ACCOUNT,
            ),
            patch(
                "app.agent.inbox_agent.fetch_unread",
                new_callable=AsyncMock,
                return_value=[inbound],
            ),
            patch(
                "app.agent.inbox_agent.chat_completion",
                new_callable=AsyncMock,
                return_value=llm_resp,
            ),
            patch(
                "app.agent.inbox_agent.drafts.create_draft",
                new_callable=AsyncMock,
                return_value={"id": "draft-123", "status": "draft"},
            ) as mock_create_draft,
            patch(
                "app.agent.inbox_agent.mark_seen",
                new_callable=AsyncMock,
            ) as mock_seen,
            patch("app.agent.inbox_agent.get_db", new_callable=AsyncMock) as mock_db,
        ):
            db_instance = AsyncMock()
            mock_db.return_value = db_instance
            db_instance.execute.return_value = AsyncMock(fetchone=AsyncMock(return_value=None))

            agent = InboxAgent(MagicMock())
            results = await agent.poll_once()

            assert len(results) == 1
            assert results[0]["action"] == "draft_for_review"
            mock_create_draft.assert_called_once()
            call_kwargs = mock_create_draft.call_args[1]
            assert call_kwargs["account_id"] == "acc-123"
            assert call_kwargs["to_addr"] == "visitor@example.com"
            assert call_kwargs["created_by"] == "inbox-agent"
            assert call_kwargs["metadata"]["classification"] == "draft_for_review"
            mock_seen.assert_called_once()

    @pytest.mark.asyncio
    async def test_poll_escalate_creates_draft(self):
        inbound = _make_inbound(subject="Legal question", text_body="What about my visa?")
        llm_resp = _make_llm_response("escalate", 0.3, None)

        with (
            patch.dict(os.environ, {
                "AGENT_ACCOUNT_ID": "acc-123",
                "AGENT_ESCALATION_EMAIL": "tyler@loftly.com",
                "AGENT_SEND_FROM": "hello@loftly.com",
            }),
            patch(
                "app.agent.inbox_agent.credential_store.get_account_with_credentials",
                new_callable=AsyncMock,
                return_value=MOCK_ACCOUNT,
            ),
            patch(
                "app.agent.inbox_agent.fetch_unread",
                new_callable=AsyncMock,
                return_value=[inbound],
            ),
            patch(
                "app.agent.inbox_agent.chat_completion",
                new_callable=AsyncMock,
                return_value=llm_resp,
            ),
            patch(
                "app.agent.inbox_agent.drafts.create_draft",
                new_callable=AsyncMock,
                return_value={"id": "draft-esc", "status": "draft"},
            ) as mock_create_draft,
            patch(
                "app.agent.inbox_agent.mark_seen",
                new_callable=AsyncMock,
            ) as mock_seen,
            patch("app.agent.inbox_agent.get_db", new_callable=AsyncMock) as mock_db,
        ):
            db_instance = AsyncMock()
            mock_db.return_value = db_instance
            db_instance.execute.return_value = AsyncMock(fetchone=AsyncMock(return_value=None))

            agent = InboxAgent(MagicMock())
            results = await agent.poll_once()

            assert len(results) == 1
            assert results[0]["action"] == "escalate"
            mock_create_draft.assert_called_once()
            call_kwargs = mock_create_draft.call_args[1]
            assert call_kwargs["metadata"]["classification"] == "escalate"
            assert call_kwargs["created_by"] == "inbox-agent"
            mock_seen.assert_called_once()

    @pytest.mark.asyncio
    async def test_skips_already_processed(self):
        inbound = _make_inbound()

        with (
            patch.dict(os.environ, {"AGENT_ACCOUNT_ID": "acc-123"}),
            patch(
                "app.agent.inbox_agent.credential_store.get_account_with_credentials",
                new_callable=AsyncMock,
                return_value=MOCK_ACCOUNT,
            ),
            patch(
                "app.agent.inbox_agent.fetch_unread",
                new_callable=AsyncMock,
                return_value=[inbound],
            ),
            patch("app.agent.inbox_agent.get_db", new_callable=AsyncMock) as mock_db,
        ):
            db_instance = AsyncMock()
            mock_db.return_value = db_instance
            # Simulate already processed (fetchone returns a row)
            db_instance.execute.return_value = AsyncMock(
                fetchone=AsyncMock(return_value={"id": "existing"})
            )

            agent = InboxAgent(MagicMock())
            results = await agent.poll_once()

            assert results == []


class TestAgentEndpoints:
    @pytest.mark.asyncio
    async def test_agent_status_disabled(self, client):
        resp = await client.get("/agent/status")
        assert resp.status_code == 200
        data = resp.json()
        assert data["enabled"] is False

    @pytest.mark.asyncio
    async def test_agent_actions_empty(self, client):
        resp = await client.get("/agent/actions")
        assert resp.status_code == 200
        assert resp.json() == []

    @pytest.mark.asyncio
    async def test_agent_poll_disabled(self, client):
        resp = await client.post("/agent/poll")
        assert resp.status_code == 503
