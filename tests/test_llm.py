import os
import pytest
from unittest.mock import patch, AsyncMock, MagicMock
import httpx

from app.agent.llm import chat_completion, LLMResponse


class TestChatCompletion:
    @pytest.mark.asyncio
    async def test_missing_api_key_raises(self):
        with patch.dict(os.environ, {}, clear=True):
            os.environ.pop("OPENROUTER_API_KEY", None)
            with pytest.raises(RuntimeError, match="OPENROUTER_API_KEY"):
                await chat_completion("system", "user")

    @pytest.mark.asyncio
    async def test_successful_completion(self):
        mock_response = MagicMock()
        mock_response.json.return_value = {
            "choices": [{"message": {"content": "Hello back"}}],
            "model": "test-model",
            "usage": {"prompt_tokens": 10, "completion_tokens": 5},
        }
        mock_response.raise_for_status = MagicMock()

        mock_client = AsyncMock()
        mock_client.post.return_value = mock_response
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=False)

        with (
            patch.dict(os.environ, {"OPENROUTER_API_KEY": "test-key"}),
            patch("app.agent.llm.httpx.AsyncClient", return_value=mock_client),
        ):
            result = await chat_completion("You are helpful", "Hi there")
            assert isinstance(result, LLMResponse)
            assert result.content == "Hello back"
            assert result.model == "test-model"

            call_args = mock_client.post.call_args
            assert call_args[1]["headers"]["Authorization"] == "Bearer test-key"
            body = call_args[1]["json"]
            assert body["messages"][0]["content"] == "You are helpful"
            assert body["messages"][1]["content"] == "Hi there"

    @pytest.mark.asyncio
    async def test_uses_env_model(self):
        mock_response = MagicMock()
        mock_response.json.return_value = {
            "choices": [{"message": {"content": "ok"}}],
            "model": "custom/model",
            "usage": {},
        }
        mock_response.raise_for_status = MagicMock()

        mock_client = AsyncMock()
        mock_client.post.return_value = mock_response
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=False)

        with (
            patch.dict(os.environ, {
                "OPENROUTER_API_KEY": "test-key",
                "OPENROUTER_MODEL": "custom/model",
            }),
            patch("app.agent.llm.httpx.AsyncClient", return_value=mock_client),
        ):
            result = await chat_completion("sys", "usr")
            body = mock_client.post.call_args[1]["json"]
            assert body["model"] == "custom/model"

    @pytest.mark.asyncio
    async def test_explicit_model_overrides_env(self):
        mock_response = MagicMock()
        mock_response.json.return_value = {
            "choices": [{"message": {"content": "ok"}}],
            "model": "explicit/model",
            "usage": {},
        }
        mock_response.raise_for_status = MagicMock()

        mock_client = AsyncMock()
        mock_client.post.return_value = mock_response
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=False)

        with (
            patch.dict(os.environ, {
                "OPENROUTER_API_KEY": "test-key",
                "OPENROUTER_MODEL": "env/model",
            }),
            patch("app.agent.llm.httpx.AsyncClient", return_value=mock_client),
        ):
            result = await chat_completion("sys", "usr", model="explicit/model")
            body = mock_client.post.call_args[1]["json"]
            assert body["model"] == "explicit/model"
