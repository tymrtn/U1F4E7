# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import json
import typer
from typing import Optional

app = typer.Typer(help="Send emails")


@app.command()
def send(
    to: str = typer.Option(..., "--to", help="Recipient email address"),
    subject: str = typer.Option(..., "--subject", help="Email subject"),
    body: str = typer.Option(..., "--body", help="Plain text body"),
    account: Optional[str] = typer.Option(None, "--account", "-a"),
    output_json: bool = typer.Option(False, "--json"),
):
    """Send an email immediately."""
    from cli.config import setup_db, get_account_id
    setup_db()
    aid = get_account_id(account)
    if not aid:
        typer.echo("No account specified. Use --account or set ENVELOPE_ACCOUNT_ID.", err=True)
        raise typer.Exit(1)

    async def _run():
        from app.db import init_db
        from app.credentials.store import get_account_with_credentials
        from app.transport.smtp import build_mime_message, send_message, SmtpSendError
        from app import messages
        await init_db()

        acct = await get_account_with_credentials(aid)
        if not acct:
            typer.echo(f"Account not found: {aid}", err=True)
            raise typer.Exit(1)

        from_addr = acct["username"]
        msg = build_mime_message(
            from_addr=from_addr,
            to_addr=to,
            subject=subject,
            text=body,
            display_name=acct.get("display_name"),
        )
        record = await messages.create_message(
            account_id=aid,
            from_addr=from_addr,
            to_addr=to,
            subject=subject,
            text_content=body,
        )
        try:
            smtp_id = await send_message(acct, msg, pool=None)
            await messages.mark_sent(record["id"], smtp_id)
            return {"status": "sent", "id": record["id"], "message_id": smtp_id}
        except SmtpSendError as e:
            await messages.mark_failed(record["id"], e.message)
            return {"status": "failed", "error": e.message}

    result = asyncio.run(_run())

    if output_json:
        typer.echo(json.dumps(result, indent=2))
    else:
        if result["status"] == "sent":
            typer.echo(f"Sent: {result['id']}")
        else:
            typer.echo(f"Failed: {result['error']}", err=True)
            raise typer.Exit(1)
