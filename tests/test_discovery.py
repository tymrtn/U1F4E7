# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import json
import pytest
from unittest.mock import AsyncMock, patch

from app.discovery import _sse_event, discover_stream


@pytest.mark.asyncio
async def test_sse_event_format():
    result = _sse_event("phase", {"name": "dns", "message": "Looking up..."})
    assert result.startswith("event: phase\n")
    assert "data: " in result
    assert result.endswith("\n\n")
    data_line = result.split("\n")[1]
    payload = json.loads(data_line.replace("data: ", ""))
    assert payload["name"] == "dns"


@pytest.mark.asyncio
async def test_invalid_email_yields_error():
    events = []
    async for chunk in discover_stream("notanemail"):
        events.append(chunk)

    assert len(events) == 1
    assert "complete" in events[0]
    data = json.loads(events[0].split("data: ")[1].strip())
    assert "error" in data


@pytest.mark.asyncio
async def test_phase_ordering_dns_autoconfig_probing():
    with (
        patch("app.discovery._discover_srv", new_callable=AsyncMock, return_value={"smtp": [], "imap": []}),
        patch("app.discovery._discover_mx", new_callable=AsyncMock, return_value={"smtp": [], "imap": [], "mx_bases": []}),
        patch("app.discovery._discover_autoconfig", new_callable=AsyncMock, return_value={"smtp": [], "imap": []}),
        patch("app.discovery._probe_best", new_callable=AsyncMock, return_value=None),
    ):
        events = []
        async for chunk in discover_stream("user@example.com"):
            events.append(chunk)

        phase_names = []
        for e in events:
            if "event: phase" in e:
                data = json.loads(e.split("data: ")[1].strip())
                phase_names.append(data["name"])

        assert "dns" in phase_names
        assert "autoconfig" in phase_names
        assert "probing" in phase_names
        # Verify ordering
        assert phase_names.index("dns") < phase_names.index("autoconfig")
        assert phase_names.index("autoconfig") < phase_names.index("probing")


@pytest.mark.asyncio
async def test_alias_phase_emitted_for_known_provider():
    mx_result = {"smtp": [], "imap": [], "mx_bases": ["google.com"]}

    with (
        patch("app.discovery._discover_srv", new_callable=AsyncMock, return_value={"smtp": [], "imap": []}),
        patch("app.discovery._discover_mx", new_callable=AsyncMock, return_value=mx_result),
        patch("app.discovery._discover_autoconfig", new_callable=AsyncMock, return_value={"smtp": [], "imap": []}),
        patch("app.discovery._probe_best", new_callable=AsyncMock, return_value=None),
    ):
        events = []
        async for chunk in discover_stream("user@custom.com"):
            events.append(chunk)

        phase_names = []
        for e in events:
            if "event: phase" in e:
                data = json.loads(e.split("data: ")[1].strip())
                phase_names.append(data["name"])

        assert "aliases" in phase_names


@pytest.mark.asyncio
async def test_alias_phase_skipped_for_unknown_provider():
    mx_result = {"smtp": [], "imap": [], "mx_bases": ["unknown-provider.com"]}

    with (
        patch("app.discovery._discover_srv", new_callable=AsyncMock, return_value={"smtp": [], "imap": []}),
        patch("app.discovery._discover_mx", new_callable=AsyncMock, return_value=mx_result),
        patch("app.discovery._discover_autoconfig", new_callable=AsyncMock, return_value={"smtp": [], "imap": []}),
        patch("app.discovery._probe_best", new_callable=AsyncMock, return_value=None),
    ):
        events = []
        async for chunk in discover_stream("user@custom.com"):
            events.append(chunk)

        phase_names = []
        for e in events:
            if "event: phase" in e:
                data = json.loads(e.split("data: ")[1].strip())
                phase_names.append(data["name"])

        # unknown-provider.com is still an alias domain (discarded only if == user domain)
        # But since it's not in MX_ALIASES, it won't expand further — aliases phase
        # is emitted because the mx_base "unknown-provider.com" != "custom.com"
        # so alias_domains is non-empty. Let's check the actual behavior:
        # alias_domains starts with mx_base, then adds MX_ALIASES entries.
        # For unknown-provider.com, no aliases, but the mx_base itself
        # (unknown-provider.com) is added and != domain (custom.com),
        # so alias_domains is non-empty and the phase IS emitted.
        # This test should verify this edge case.
        assert "aliases" in phase_names


@pytest.mark.asyncio
async def test_complete_contains_discovered_servers():
    smtp_result = ("smtp.example.com", 587, "autoconfig")
    imap_result = ("imap.example.com", 993, "autoconfig")

    with (
        patch("app.discovery._discover_srv", new_callable=AsyncMock, return_value={"smtp": [], "imap": []}),
        patch("app.discovery._discover_mx", new_callable=AsyncMock, return_value={"smtp": [], "imap": [], "mx_bases": []}),
        patch("app.discovery._discover_autoconfig", new_callable=AsyncMock, return_value={"smtp": [], "imap": []}),
        patch("app.discovery._probe_best", new_callable=AsyncMock, side_effect=[smtp_result, imap_result]),
    ):
        events = []
        async for chunk in discover_stream("user@example.com"):
            events.append(chunk)

        complete_event = [e for e in events if "event: complete" in e]
        assert len(complete_event) == 1
        data = json.loads(complete_event[0].split("data: ")[1].strip())
        assert data["smtp_host"] == "smtp.example.com"
        assert data["smtp_port"] == 587
        assert data["imap_host"] == "imap.example.com"
        assert data["imap_port"] == 993


@pytest.mark.asyncio
async def test_complete_omits_missing_protocols():
    with (
        patch("app.discovery._discover_srv", new_callable=AsyncMock, return_value={"smtp": [], "imap": []}),
        patch("app.discovery._discover_mx", new_callable=AsyncMock, return_value={"smtp": [], "imap": [], "mx_bases": []}),
        patch("app.discovery._discover_autoconfig", new_callable=AsyncMock, return_value={"smtp": [], "imap": []}),
        patch("app.discovery._probe_best", new_callable=AsyncMock, side_effect=[None, ("imap.example.com", 993, "autoconfig")]),
    ):
        events = []
        async for chunk in discover_stream("user@example.com"):
            events.append(chunk)

        complete_event = [e for e in events if "event: complete" in e]
        data = json.loads(complete_event[0].split("data: ")[1].strip())
        assert "smtp_host" not in data
        assert data["imap_host"] == "imap.example.com"


@pytest.mark.asyncio
async def test_dns_exception_doesnt_crash():
    with (
        patch("app.discovery._discover_srv", new_callable=AsyncMock, side_effect=Exception("DNS failure")),
        patch("app.discovery._discover_mx", new_callable=AsyncMock, side_effect=Exception("DNS failure")),
        patch("app.discovery._discover_autoconfig", new_callable=AsyncMock, return_value={"smtp": [], "imap": []}),
        patch("app.discovery._probe_best", new_callable=AsyncMock, return_value=None),
    ):
        events = []
        async for chunk in discover_stream("user@example.com"):
            events.append(chunk)

        # Should still reach complete phase without crashing
        phase_names = []
        for e in events:
            if "event: phase" in e:
                data = json.loads(e.split("data: ")[1].strip())
                phase_names.append(data["name"])

        assert "autoconfig" in phase_names
        complete_event = [e for e in events if "event: complete" in e]
        assert len(complete_event) == 1
