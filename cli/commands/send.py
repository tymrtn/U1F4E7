# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import base64
import json
import mimetypes
import os
import typer
from typing import Optional

app = typer.Typer(help="Send emails")


@app.command()
def send(
    to: str = typer.Option(..., "--to", help="Recipient email address"),
    subject: str = typer.Option(..., "--subject", help="Email subject"),
    body: str = typer.Option(..., "--body", help="Plain text body"),
    account: Optional[str] = typer.Option(None, "--account", "-a"),
    attach: Optional[list[str]] = typer.Option(None, "--attach", "-f", help="File path to attach (repeatable)"),
    output_json: bool = typer.Option(False, "--json"),
):
    """Send an email immediately."""
    from cli.config import setup_db, get_account_id
    setup_db()
    aid = get_account_id(account)
    if not aid:
        typer.echo("No account specified. Use --account or set ENVELOPE_ACCOUNT_ID.", err=True)
        raise typer.Exit(1)

    # Build attachments from file paths
    attachments = None
    if attach:
        attachments = []
        for path in attach:
            if not os.path.isfile(path):
                typer.echo(f"File not found: {path}", err=True)
                raise typer.Exit(1)
            with open(path, "rb") as f:
                data = f.read()
            filename = os.path.basename(path)
            content_type = mimetypes.guess_type(filename)[0] or "application/octet-stream"
            attachments.append({
                "filename": filename,
                "content": base64.b64encode(data).decode(),
                "content_type": content_type,
            })

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
            attachments=attachments,
        )

        att_meta = None
        if attachments:
            att_meta = [{"filename": a["filename"], "content_type": a.get("content_type"), "size_bytes": len(base64.b64decode(a["content"]))} for a in attachments]

        record = await messages.create_message(
            account_id=aid,
            from_addr=from_addr,
            to_addr=to,
            subject=subject,
            text_content=body,
            attachments_meta=att_meta,
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
