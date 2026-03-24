# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import pytest
from unittest.mock import patch, AsyncMock

from app.main import _strip_duplicate_re, _parse_addresses, _build_references


# --- Unit tests for reply helpers ---

class TestStripDuplicateRe:
    def test_single_re(self):
        assert _strip_duplicate_re("Re: Hello") == "Re: Hello"

    def test_double_re(self):
        assert _strip_duplicate_re("Re: Re: Hello") == "Re: Hello"

    def test_triple_re(self):
        assert _strip_duplicate_re("Re: Re: Re: Hello") == "Re: Hello"

    def test_no_re(self):
        assert _strip_duplicate_re("Hello") == "Re: Hello"

    def test_case_insensitive(self):
        assert _strip_duplicate_re("RE: re: Hello") == "Re: Hello"

    def test_empty(self):
        assert _strip_duplicate_re("") == "Re:"


class TestParseAddresses:
    def test_single_bare(self):
        assert _parse_addresses("alice@example.com") == ["alice@example.com"]

    def test_single_display_name(self):
        assert _parse_addresses("Alice <alice@example.com>") == ["alice@example.com"]

    def test_multiple(self):
        result = _parse_addresses("Alice <alice@example.com>, Bob <bob@example.com>")
        assert result == ["alice@example.com", "bob@example.com"]

    def test_empty(self):
        assert _parse_addresses("") == []

    def test_lowercased(self):
        assert _parse_addresses("Alice@EXAMPLE.COM") == ["alice@example.com"]


class TestBuildReferences:
    def test_no_prior_references(self):
        assert _build_references(None, "<msg1@ex.com>") == "<msg1@ex.com>"

    def test_with_prior_references(self):
        result = _build_references("<orig@ex.com>", "<msg1@ex.com>")
        assert result == "<orig@ex.com> <msg1@ex.com>"

    def test_with_chain(self):
        result = _build_references("<a@ex.com> <b@ex.com>", "<c@ex.com>")
        assert result == "<a@ex.com> <b@ex.com> <c@ex.com>"


# --- Integration tests for reply endpoints ---

MOCK_ORIGINAL = {
    "uid": "42",
    "message_id": "<original@example.com>",
    "from_addr": "Alice <alice@example.com>",
    "to_addr": "me@mycompany.com",
    "cc_addr": "bob@example.com, carol@example.com",
    "subject": "Hello there",
    "date": "Mon, 01 Jan 2026 12:00:00 +0000",
    "in_reply_to": None,
    "references": None,
    "text_body": "Hello!\nHow are you?",
    "html_body": None,
    "attachments": [],
}

MOCK_ACCOUNT = {
    "id": "acct1",
    "username": "me@mycompany.com",
    "domain": "mycompany.com",
    "display_name": None,
    "signature_text": None,
    "signature_html": None,
    "auto_send_threshold": 0.85,
    "review_threshold": 0.50,
    "delay_margin": 0.10,
    "delay_minutes": 15,
    "imap_host": "imap.example.com",
    "imap_port": 993,
    "effective_imap_username": "me@mycompany.com",
    "effective_imap_password": "pass",
}

MOCK_ROUTE_RESULT = {
    "draft_id": "d1",
    "status": "sent",
    "routing_status": "sent",
    "to": "alice@example.com",
    "subject": "Re: Hello there",
    "review_url": "/review?highlight=d1",
    "send_after": None,
}


@pytest.fixture
def _mock_deps():
    """Patch external dependencies for reply endpoint tests."""
    with (
        patch("app.main.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=MOCK_ACCOUNT) as mock_creds,
        patch("app.main.fetch_message", new_callable=AsyncMock, return_value=MOCK_ORIGINAL) as mock_fetch,
        patch("app.main._route_compose_request", new_callable=AsyncMock, return_value=MOCK_ROUTE_RESULT) as mock_route,
    ):
        yield {
            "creds": mock_creds,
            "fetch": mock_fetch,
            "route": mock_route,
        }


@pytest.fixture
def client():
    from fastapi.testclient import TestClient
    from app.main import app
    return TestClient(app)


class TestReplyEndpoint:
    def test_reply_basic(self, client, _mock_deps):
        resp = client.post(
            "/accounts/acct1/reply/42",
            json={"body": "Thanks!", "confidence": 0.9},
        )
        assert resp.status_code == 201
        result = resp.json()
        assert result["draft_id"] == "d1"
        assert result["status"] == "sent"

        # Verify compose was called with correct args
        call_args = _mock_deps["route"].call_args
        compose_data = call_args[0][2]  # 3rd positional arg
        assert compose_data.to == "alice@example.com"
        assert compose_data.subject == "Re: Hello there"
        assert compose_data.in_reply_to == "<original@example.com>"
        assert "> Hello!" in compose_data.body
        assert "> How are you?" in compose_data.body
        assert "Thanks!" in compose_data.body

    def test_reply_requires_scoring(self, client, _mock_deps):
        resp = client.post(
            "/accounts/acct1/reply/42",
            json={"body": "Thanks!"},
        )
        assert resp.status_code == 422

    def test_reply_strips_duplicate_re(self, client, _mock_deps):
        original_with_re = {**MOCK_ORIGINAL, "subject": "Re: Re: Hello there"}
        _mock_deps["fetch"].return_value = original_with_re
        resp = client.post(
            "/accounts/acct1/reply/42",
            json={"body": "Thanks!", "confidence": 0.9},
        )
        assert resp.status_code == 201
        compose_data = _mock_deps["route"].call_args[0][2]
        assert compose_data.subject == "Re: Hello there"

    def test_reply_preserves_references_chain(self, client, _mock_deps):
        original_with_refs = {
            **MOCK_ORIGINAL,
            "references": "<root@example.com> <mid@example.com>",
        }
        _mock_deps["fetch"].return_value = original_with_refs
        resp = client.post(
            "/accounts/acct1/reply/42",
            json={"body": "Thanks!", "confidence": 0.9},
        )
        assert resp.status_code == 201
        compose_data = _mock_deps["route"].call_args[0][2]
        refs = compose_data.metadata["references"]
        assert refs == "<root@example.com> <mid@example.com> <original@example.com>"

    def test_reply_404_no_account(self, client):
        with patch("app.main.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=None):
            resp = client.post(
                "/accounts/acct1/reply/42",
                json={"body": "Thanks!", "confidence": 0.9},
            )
            assert resp.status_code == 404

    def test_reply_404_no_message(self, client):
        with (
            patch("app.main.credential_store.get_account_with_credentials", new_callable=AsyncMock, return_value=MOCK_ACCOUNT),
            patch("app.main.fetch_message", new_callable=AsyncMock, return_value=None),
        ):
            resp = client.post(
                "/accounts/acct1/reply/42",
                json={"body": "Thanks!", "confidence": 0.9},
            )
            assert resp.status_code == 404


class TestReplyAllEndpoint:
    def test_reply_all_includes_cc(self, client, _mock_deps):
        resp = client.post(
            "/accounts/acct1/reply-all/42",
            json={"body": "Thanks all!", "confidence": 0.9},
        )
        assert resp.status_code == 201
        compose_data = _mock_deps["route"].call_args[0][2]
        assert compose_data.to == "alice@example.com"
        # CC should include bob and carol (from original To/CC) minus self
        assert "bob@example.com" in compose_data.cc
        assert "carol@example.com" in compose_data.cc
        # Self should NOT be in CC
        assert "me@mycompany.com" not in compose_data.cc

    def test_reply_all_excludes_self_from_cc(self, client, _mock_deps):
        original = {
            **MOCK_ORIGINAL,
            "to_addr": "me@mycompany.com, dave@example.com",
            "cc_addr": "me@mycompany.com",
        }
        _mock_deps["fetch"].return_value = original
        resp = client.post(
            "/accounts/acct1/reply-all/42",
            json={"body": "Thanks!", "confidence": 0.9},
        )
        assert resp.status_code == 201
        compose_data = _mock_deps["route"].call_args[0][2]
        assert "me@mycompany.com" not in (compose_data.cc or "")
        assert "dave@example.com" in compose_data.cc

    def test_reply_all_merges_explicit_cc(self, client, _mock_deps):
        resp = client.post(
            "/accounts/acct1/reply-all/42",
            json={"body": "Thanks!", "cc": "extra@example.com", "confidence": 0.9},
        )
        assert resp.status_code == 201
        compose_data = _mock_deps["route"].call_args[0][2]
        assert "extra@example.com" in compose_data.cc
        assert "bob@example.com" in compose_data.cc

    def test_reply_all_no_cc_when_only_sender_and_self(self, client, _mock_deps):
        original = {
            **MOCK_ORIGINAL,
            "to_addr": "me@mycompany.com",
            "cc_addr": "",
        }
        _mock_deps["fetch"].return_value = original
        resp = client.post(
            "/accounts/acct1/reply-all/42",
            json={"body": "Thanks!", "confidence": 0.9},
        )
        assert resp.status_code == 201
        compose_data = _mock_deps["route"].call_args[0][2]
        assert compose_data.cc is None

    def test_reply_all_with_attribution(self, client, _mock_deps):
        resp = client.post(
            "/accounts/acct1/reply-all/42",
            json={
                "body": "Thanks!",
                "attribution": {"attributes": ["reply", "known_contact", "low_stakes"]},
            },
        )
        assert resp.status_code == 201


class TestReplyTextBodyConsistency:
    """Tests for text/body field consistency between /send and /reply endpoints.

    The /send endpoint uses `text`, while /reply historically used `body`.
    Both endpoints now accept both fields, with `text` preferred.
    """

    def test_reply_with_text_field(self, client, _mock_deps):
        """Reply endpoint accepts `text` (consistent with /send)."""
        resp = client.post(
            "/accounts/acct1/reply/42",
            json={"text": "Thanks via text!", "confidence": 0.9},
        )
        assert resp.status_code == 201
        compose_data = _mock_deps["route"].call_args[0][2]
        assert "Thanks via text!" in compose_data.body

    def test_reply_with_body_field_still_works(self, client, _mock_deps):
        """Reply endpoint still accepts deprecated `body` for backward compat."""
        resp = client.post(
            "/accounts/acct1/reply/42",
            json={"body": "Thanks via body!", "confidence": 0.9},
        )
        assert resp.status_code == 201
        compose_data = _mock_deps["route"].call_args[0][2]
        assert "Thanks via body!" in compose_data.body

    def test_reply_text_preferred_over_body(self, client, _mock_deps):
        """When both `text` and `body` are provided, `text` wins."""
        resp = client.post(
            "/accounts/acct1/reply/42",
            json={"text": "text wins", "body": "body loses", "confidence": 0.9},
        )
        assert resp.status_code == 201
        compose_data = _mock_deps["route"].call_args[0][2]
        assert "text wins" in compose_data.body
        assert "body loses" not in compose_data.body

    def test_reply_neither_text_nor_body_fails(self, client, _mock_deps):
        """Reply with no content should fail validation."""
        resp = client.post(
            "/accounts/acct1/reply/42",
            json={"confidence": 0.9},
        )
        assert resp.status_code == 422

    def test_reply_all_with_text_field(self, client, _mock_deps):
        """Reply-all endpoint accepts `text` (consistent with /send)."""
        resp = client.post(
            "/accounts/acct1/reply-all/42",
            json={"text": "Thanks all via text!", "confidence": 0.9},
        )
        assert resp.status_code == 201
        compose_data = _mock_deps["route"].call_args[0][2]
        assert "Thanks all via text!" in compose_data.body

    def test_reply_all_text_preferred_over_body(self, client, _mock_deps):
        """Reply-all also prefers `text` over `body`."""
        resp = client.post(
            "/accounts/acct1/reply-all/42",
            json={"text": "text wins", "body": "body loses", "confidence": 0.9},
        )
        assert resp.status_code == 201
        compose_data = _mock_deps["route"].call_args[0][2]
        assert "text wins" in compose_data.body
        assert "body loses" not in compose_data.body
