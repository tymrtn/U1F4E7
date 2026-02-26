# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import json
import typer
from typing import Optional

app = typer.Typer(help="Manage email accounts")


@app.command("list")
def list_accounts(output_json: bool = typer.Option(False, "--json")):
    """List all configured accounts."""
    from cli.config import setup_db
    setup_db()

    async def _run():
        from app.db import init_db
        from app.credentials.store import list_accounts as _list
        await init_db()
        return await _list()

    accounts = asyncio.run(_run())

    if output_json:
        typer.echo(json.dumps(accounts, indent=2))
        return

    if not accounts:
        typer.echo("No accounts configured.")
        return

    try:
        from rich.table import Table
        from rich.console import Console
        console = Console()
        table = Table(title="Accounts")
        table.add_column("ID", style="dim")
        table.add_column("Name")
        table.add_column("Username")
        table.add_column("SMTP Host")
        table.add_column("Webhook")
        for a in accounts:
            table.add_row(
                a["id"][:8] + "...",
                a["name"],
                a["username"],
                a["smtp_host"],
                "yes" if a.get("webhook_url") else "no",
            )
        console.print(table)
    except ImportError:
        for a in accounts:
            typer.echo(f"{a['id']} | {a['name']} | {a['username']} | {a['smtp_host']}")


@app.command("add")
def add_account(
    name: str = typer.Option(..., prompt=True),
    host: str = typer.Option(..., prompt=True),
    username: str = typer.Option(..., prompt=True),
    password: str = typer.Option(..., prompt=True, hide_input=True),
    output_json: bool = typer.Option(False, "--json"),
):
    """Add a new email account."""
    from cli.config import setup_db
    setup_db()

    async def _run():
        from app.db import init_db
        from app.credentials.store import create_account
        await init_db()
        return await create_account(
            name=name,
            smtp_host=host,
            smtp_port=587,
            imap_host=host,
            imap_port=993,
            username=username,
            password=password,
        )

    account = asyncio.run(_run())

    if output_json:
        typer.echo(json.dumps(account, indent=2))
    else:
        typer.echo(f"Account created: {account['id']} ({account['name']})")
