# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import os
from pathlib import Path


def get_account_id(account_override: str | None = None) -> str | None:
    """Resolve account ID from CLI flag, env var, or config file."""
    if account_override:
        return account_override
    env = os.getenv("ENVELOPE_ACCOUNT_ID")
    if env:
        return env
    config_path = Path.home() / ".envelope" / "config.toml"
    if config_path.exists():
        try:
            import tomllib  # Python 3.11+
            with open(config_path, "rb") as f:
                config = tomllib.load(f)
            return config.get("default_account_id")
        except Exception:
            pass
    return None


def setup_db():
    """Initialize DB from env or default path before running CLI commands."""
    import app.db as db_module
    db_path = os.getenv("ENVELOPE_DB_PATH", "envelope.db")
    db_module.DB_PATH = db_path
    db_module._connection = None
