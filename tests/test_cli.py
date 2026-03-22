# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import os
import pytest
from unittest.mock import AsyncMock, patch




@pytest.fixture(autouse=True)
def cli_db_setup(tmp_path):
    """Set up isolated DB for CLI tests."""
    import app.db as db_module
    db_path = str(tmp_path / "cli_test.db")
    os.environ["ENVELOPE_DB_PATH"] = db_path
    os.environ.setdefault("ENVELOPE_SECRET_KEY", "test-key-for-ci")
    db_module._connection = None
    db_module.DB_PATH = db_path
    yield
    import asyncio
    from app.db import close_db
    asyncio.get_event_loop().run_until_complete(close_db())
    if os.path.exists(db_path):
        os.unlink(db_path)


def test_cli_accounts_list_empty(tmp_path):
    """envelope accounts list returns empty output when no accounts exist."""
    pytest.importorskip("typer")
    from typer.testing import CliRunner
    from cli.main import app

    runner = CliRunner()
    result = runner.invoke(app, ["accounts", "list"])
    assert result.exit_code == 0
    assert "No accounts" in result.output


def test_cli_accounts_list_json(tmp_path):
    """envelope accounts list --json returns valid JSON array."""
    pytest.importorskip("typer")
    import json
    from typer.testing import CliRunner
    from cli.main import app

    runner = CliRunner()
    result = runner.invoke(app, ["accounts", "list", "--json"])
    assert result.exit_code == 0
    data = json.loads(result.output)
    assert isinstance(data, list)


def test_cli_policy_show_no_policy(tmp_path):
    """envelope policy show reports no policy when account has none."""
    pytest.importorskip("typer")
    import asyncio
    from typer.testing import CliRunner
    from cli.main import app
    from cli.config import setup_db
    import app.db as db_module

    db_path = os.environ["ENVELOPE_DB_PATH"]
    db_module.DB_PATH = db_path
    db_module._connection = None

    # Create an account first
    async def _setup():
        from app.db import init_db
        from app.credentials.store import create_account
        await init_db()
        return await create_account(
            name="CLI Test",
            smtp_host="smtp.example.com",
            smtp_port=587,
            imap_host="imap.example.com",
            imap_port=993,
            username="user@example.com",
            password="secret",
        )

    account = asyncio.run(_setup())

    runner = CliRunner()
    result = runner.invoke(app, ["policy", "show", "--account", account["id"]])
    assert result.exit_code == 0
    assert "No domain policy" in result.output


def test_cli_actions_tail_empty(tmp_path):
    """envelope actions tail returns empty when no actions logged."""
    pytest.importorskip("typer")
    import asyncio
    from typer.testing import CliRunner
    from cli.main import app
    import app.db as db_module

    db_path = os.environ["ENVELOPE_DB_PATH"]
    db_module.DB_PATH = db_path
    db_module._connection = None

    async def _setup():
        from app.db import init_db
        from app.credentials.store import create_account
        await init_db()
        return await create_account(
            name="Actions Test",
            smtp_host="smtp.example.com",
            smtp_port=587,
            imap_host="imap.example.com",
            imap_port=993,
            username="user@example.com",
            password="secret",
        )

    account = asyncio.run(_setup())

    runner = CliRunner()
    result = runner.invoke(app, ["actions", "tail", "--account", account["id"]])
    assert result.exit_code == 0
    assert "No actions" in result.output
