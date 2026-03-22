# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import json
import typer
from typing import Optional

app = typer.Typer(help="View action log")


def _resolve_account(account: Optional[str]) -> str:
    from cli.config import get_account_id
    aid = get_account_id(account)
    if not aid:
        typer.echo("No account specified. Use --account or set ENVELOPE_ACCOUNT_ID.", err=True)
        raise typer.Exit(1)
    return aid


@app.command("tail")
def tail_actions(
    account: Optional[str] = typer.Option(None, "--account", "-a"),
    limit: int = typer.Option(20, "--limit", "-n"),
    output_json: bool = typer.Option(False, "--json"),
):
    """Show recent actions for an account."""
    from cli.config import setup_db
    setup_db()
    aid = _resolve_account(account)

    async def _run():
        from app.db import init_db
        from app.services.actions import list_actions
        await init_db()
        return await list_actions(aid, limit=limit)

    entries = asyncio.run(_run())

    if output_json:
        typer.echo(json.dumps(entries, indent=2))
        return

    if not entries:
        typer.echo("No actions logged yet.")
        return

    for e in entries:
        typer.echo(
            f"{e['created_at'][:19]} | {e['action_type']:20} | conf={e['confidence']:.2f} | {e['action_taken'][:50]}"
        )


@app.command("log")
def log_action(
    action_type: str = typer.Option(..., "--type", help="inbound_route|draft_approve|draft_reject|send_decision|escalate|trash"),
    confidence: float = typer.Option(..., "--confidence"),
    justification: str = typer.Option(..., "--why"),
    action_taken: str = typer.Option(..., "--what"),
    account: Optional[str] = typer.Option(None, "--account", "-a"),
    output_json: bool = typer.Option(False, "--json"),
):
    """Manually log an action entry."""
    from cli.config import setup_db
    setup_db()
    aid = _resolve_account(account)

    async def _run():
        from app.db import init_db
        from app.services.actions import log_action as _log
        await init_db()
        return await _log(
            account_id=aid,
            action_type=action_type,
            confidence=confidence,
            justification=justification,
            action_taken=action_taken,
        )

    entry = asyncio.run(_run())

    if output_json:
        typer.echo(json.dumps(entry, indent=2))
    else:
        typer.echo(f"Logged: {entry['id']}")
