# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import json
import typer
from typing import Optional

app = typer.Typer(help="Manage account policies")


def _resolve_account(account: Optional[str]) -> str:
    from cli.config import get_account_id
    aid = get_account_id(account)
    if not aid:
        typer.echo("No account specified. Use --account or set ENVELOPE_ACCOUNT_ID.", err=True)
        raise typer.Exit(1)
    return aid


@app.command("show")
def show_policy(
    account: Optional[str] = typer.Option(None, "--account", "-a"),
    output_json: bool = typer.Option(False, "--json"),
):
    """Show domain policy for an account."""
    from cli.config import setup_db
    setup_db()
    aid = _resolve_account(account)

    async def _run():
        from app.db import init_db
        from app.services.policy import get_domain_policy, list_address_policies
        await init_db()
        return await get_domain_policy(aid), await list_address_policies(aid)

    domain_policy, address_policies = asyncio.run(_run())

    if output_json:
        typer.echo(json.dumps({"domain_policy": domain_policy, "address_policies": address_policies}, indent=2))
        return

    if not domain_policy:
        typer.echo("No domain policy set. Run: envelope policy set-domain")
        return

    typer.echo(f"Domain Policy: {domain_policy['name']}")
    typer.echo(f"  Tone: {domain_policy.get('tone', 'not set')}")
    typer.echo(f"  Style: {domain_policy.get('style', 'not set')}")
    typer.echo(f"  Values: {', '.join(domain_policy.get('values') or [])}")
    typer.echo(f"\nAddress Policies ({len(address_policies)}):")
    for p in address_policies:
        typer.echo(f"  {p['pattern']} — {p.get('purpose', 'no purpose')}")


@app.command("set-domain")
def set_domain_policy(
    name: str = typer.Option(..., prompt=True),
    tone: Optional[str] = typer.Option(None),
    style: Optional[str] = typer.Option(None),
    account: Optional[str] = typer.Option(None, "--account", "-a"),
    output_json: bool = typer.Option(False, "--json"),
):
    """Set (upsert) the domain policy for an account."""
    from cli.config import setup_db
    setup_db()
    aid = _resolve_account(account)

    async def _run():
        from app.db import init_db
        from app.services.policy import upsert_domain_policy
        await init_db()
        return await upsert_domain_policy(aid, name=name, tone=tone, style=style)

    policy = asyncio.run(_run())

    if output_json:
        typer.echo(json.dumps(policy, indent=2))
    else:
        typer.echo(f"Domain policy set: {policy['name']}")


@app.command("add-address")
def add_address_policy(
    pattern: str = typer.Option(..., prompt=True),
    purpose: Optional[str] = typer.Option(None),
    account: Optional[str] = typer.Option(None, "--account", "-a"),
    output_json: bool = typer.Option(False, "--json"),
):
    """Add an address policy pattern."""
    from cli.config import setup_db
    setup_db()
    aid = _resolve_account(account)

    async def _run():
        from app.db import init_db
        from app.services.policy import upsert_address_policy
        await init_db()
        return await upsert_address_policy(aid, pattern, purpose=purpose)

    policy = asyncio.run(_run())

    if output_json:
        typer.echo(json.dumps(policy, indent=2))
    else:
        typer.echo(f"Address policy added: {policy['pattern']}")
