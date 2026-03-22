# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import json
import typer
from typing import Optional

app = typer.Typer(help="Manage drafts")


def _resolve_account(account: Optional[str]) -> str:
    from cli.config import get_account_id
    aid = get_account_id(account)
    if not aid:
        typer.echo("No account specified. Use --account or set ENVELOPE_ACCOUNT_ID.", err=True)
        raise typer.Exit(1)
    return aid


@app.command("list")
def list_drafts(
    account: Optional[str] = typer.Option(None, "--account", "-a"),
    status: Optional[str] = typer.Option(None, "--status"),
    output_json: bool = typer.Option(False, "--json"),
):
    """List drafts for an account."""
    from cli.config import setup_db
    setup_db()
    aid = _resolve_account(account)

    async def _run():
        from app.db import init_db
        from app import drafts
        await init_db()
        return await drafts.list_drafts(aid, status=status)

    items = asyncio.run(_run())

    if output_json:
        typer.echo(json.dumps(items, indent=2))
        return

    if not items:
        typer.echo("No drafts found.")
        return

    for d in items:
        typer.echo(f"{d['id'][:8]}... | {d['status']} | to: {d['to_addr']} | {d.get('subject', '(no subject)')}")


@app.command("approve")
def approve_draft(
    draft_id: str = typer.Argument(...),
    account: Optional[str] = typer.Option(None, "--account", "-a"),
    output_json: bool = typer.Option(False, "--json"),
):
    """Approve and send a draft."""
    from cli.config import setup_db, get_account_id
    setup_db()
    aid = _resolve_account(account)

    async def _run():
        from app.db import init_db
        from app import drafts as drafts_module
        from app.credentials.store import get_account_with_credentials
        from app.transport.smtp import build_mime_message, send_message, SmtpSendError
        from app import messages
        from datetime import datetime, timezone
        await init_db()

        draft = await drafts_module.get_draft(draft_id)
        if not draft or draft["account_id"] != aid:
            return {"error": "Draft not found"}
        if draft["status"] != "draft":
            return {"error": f"Cannot send draft with status '{draft['status']}'"}

        acct = await get_account_with_credentials(aid)
        if not acct:
            return {"error": "Account not found"}

        meta = draft.get("metadata") or {}
        meta["approved_at"] = datetime.now(timezone.utc).isoformat()
        meta["approved_by"] = "cli"
        await drafts_module.update_draft(draft_id, metadata=meta)

        from_addr = acct["username"]
        msg = build_mime_message(
            from_addr=from_addr,
            to_addr=draft["to_addr"],
            subject=draft["subject"] or "",
            text=draft["text_content"],
            html=draft["html_content"],
            display_name=acct.get("display_name"),
        )
        record = await messages.create_message(
            account_id=aid,
            from_addr=from_addr,
            to_addr=draft["to_addr"],
            subject=draft["subject"],
        )
        try:
            smtp_id = await send_message(acct, msg, pool=None)
            await messages.mark_sent(record["id"], smtp_id)
            await drafts_module.mark_draft_sent(draft_id, record["id"])
            return {"status": "sent", "draft_id": draft_id, "message_id": record["id"]}
        except SmtpSendError as e:
            await messages.mark_failed(record["id"], e.message)
            return {"error": e.message}

    result = asyncio.run(_run())

    if output_json:
        typer.echo(json.dumps(result, indent=2))
    elif "error" in result:
        typer.echo(f"Error: {result['error']}", err=True)
        raise typer.Exit(1)
    else:
        typer.echo(f"Sent: {result['draft_id']}")


@app.command("reject")
def reject_draft(
    draft_id: str = typer.Argument(...),
    feedback: Optional[str] = typer.Option(None, "--feedback"),
    account: Optional[str] = typer.Option(None, "--account", "-a"),
):
    """Reject and discard a draft."""
    from cli.config import setup_db
    setup_db()
    aid = _resolve_account(account)

    async def _run():
        from app.db import init_db
        from app import drafts as drafts_module
        from datetime import datetime, timezone
        await init_db()

        draft = await drafts_module.get_draft(draft_id)
        if not draft or draft["account_id"] != aid:
            return {"error": "Draft not found"}

        meta = draft.get("metadata") or {}
        meta["rejected_at"] = datetime.now(timezone.utc).isoformat()
        if feedback:
            meta["rejection_feedback"] = feedback
        await drafts_module.update_draft(draft_id, metadata=meta)
        await drafts_module.discard_draft(draft_id)
        return {"status": "rejected"}

    result = asyncio.run(_run())

    if "error" in result:
        typer.echo(f"Error: {result['error']}", err=True)
        raise typer.Exit(1)
    else:
        typer.echo(f"Rejected: {draft_id}")
