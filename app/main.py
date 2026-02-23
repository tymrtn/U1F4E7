# Envelope Email - Transactional Email API

from contextlib import asynccontextmanager
from fastapi import FastAPI, Request, HTTPException
from fastapi.responses import HTMLResponse
from fastapi.staticfiles import StaticFiles
from fastapi.templating import Jinja2Templates
from fastapi.middleware.cors import CORSMiddleware
from pydantic import BaseModel
from dotenv import load_dotenv
import os
from typing import Optional

from fastapi.responses import JSONResponse
from starlette.responses import StreamingResponse

from app.db import init_db, close_db
from app.credentials import store as credential_store
from app.transport.smtp import build_mime_message, send_message, SmtpSendError
from app.transport.pool import SmtpConnectionPool
from app.transport.worker import SendWorker
from app.transport.imap import search_messages, fetch_message, list_folders, get_thread, ImapError
from app import messages
from app import drafts
from app.discovery import discover, discover_stream
from app.agent.inbox_agent import InboxAgent

load_dotenv()


@asynccontextmanager
async def lifespan(app: FastAPI):
    await init_db()
    app.state.smtp_pool = SmtpConnectionPool()
    app.state.smtp_pool.start_cleanup_task()
    app.state.send_worker = SendWorker(app.state.smtp_pool)
    await app.state.send_worker.start()

    app.state.inbox_agent = None
    if os.getenv("AGENT_ENABLED", "false").lower() == "true":
        app.state.inbox_agent = InboxAgent(app.state.smtp_pool)
        await app.state.inbox_agent.start()

    yield

    if app.state.inbox_agent:
        await app.state.inbox_agent.stop()
    await app.state.send_worker.stop()
    await app.state.smtp_pool.close_all()
    await close_db()

app = FastAPI(title="Envelope Email API", version="0.2.0", lifespan=lifespan)

# CORS for webhooks/dashboard
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

# Mount static and templates
app.mount("/static", StaticFiles(directory="static"), name="static")
templates = Jinja2Templates(directory="templates")


# --- Models ---

class SendEmail(BaseModel):
    account_id: str
    to: str
    subject: str
    from_email: Optional[str] = None
    html: Optional[str] = None
    text: Optional[str] = None
    wait: bool = True


class CreateDraft(BaseModel):
    to: str
    subject: Optional[str] = None
    text: Optional[str] = None
    html: Optional[str] = None
    in_reply_to: Optional[str] = None
    metadata: Optional[dict] = None
    created_by: Optional[str] = None


class UpdateDraft(BaseModel):
    to: Optional[str] = None
    subject: Optional[str] = None
    text: Optional[str] = None
    html: Optional[str] = None
    in_reply_to: Optional[str] = None
    metadata: Optional[dict] = None


class ScheduleDraft(BaseModel):
    send_after: Optional[str] = None
    snoozed_until: Optional[str] = None


class RejectDraft(BaseModel):
    feedback: Optional[str] = None


class CreateAccount(BaseModel):
    name: str
    # Shared credentials (common case)
    host: Optional[str] = None
    username: Optional[str] = None
    password: Optional[str] = None
    smtp_port: int = 587
    imap_port: int = 993
    # Separate credentials (override case)
    smtp_host: Optional[str] = None
    smtp_username: Optional[str] = None
    smtp_password: Optional[str] = None
    imap_host: Optional[str] = None
    imap_username: Optional[str] = None
    imap_password: Optional[str] = None
    # Optional metadata
    display_name: Optional[str] = None
    approval_required: bool = True
    # Agent thresholds
    auto_send_threshold: float = 0.85
    review_threshold: float = 0.50


class UpdateAccount(BaseModel):
    display_name: Optional[str] = None
    auto_send_threshold: Optional[float] = None
    review_threshold: Optional[float] = None


# --- Dashboard ---

@app.get("/", response_class=HTMLResponse)
async def dashboard(request: Request):
    return templates.TemplateResponse(request, "index.html")


# --- Send ---

@app.post("/send")
async def send_email(request: Request, data: SendEmail):
    account = await credential_store.get_account_with_credentials(data.account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")

    from_addr = data.from_email or account["username"]
    pool = request.app.state.smtp_pool

    # Create tracking record (store body for async sends)
    record = await messages.create_message(
        account_id=data.account_id,
        from_addr=from_addr,
        to_addr=data.to,
        subject=data.subject,
        text_content=data.text,
        html_content=data.html,
    )

    # Async mode: queue and return immediately
    if not data.wait:
        request.app.state.send_worker.notify()
        return {
            "status": "queued",
            "id": record["id"],
            "envelope": {
                "from": from_addr,
                "to": data.to,
                "subject": data.subject,
            },
        }

    # Sync mode (default): send now and wait for result
    msg = build_mime_message(
        from_addr=from_addr,
        to_addr=data.to,
        subject=data.subject,
        text=data.text,
        html=data.html,
        display_name=account.get("display_name"),
    )

    try:
        smtp_message_id = await send_message(account, msg, pool=pool)
    except SmtpSendError as e:
        await messages.mark_failed(record["id"], e.message)
        return JSONResponse(
            status_code=502,
            content={"error": e.message, "error_type": e.error_type},
        )

    await messages.mark_sent(record["id"], smtp_message_id)

    return {
        "status": "sent",
        "id": record["id"],
        "message_id": smtp_message_id,
        "envelope": {
            "from": from_addr,
            "to": data.to,
            "subject": data.subject,
        },
    }


# --- Messages ---

@app.get("/messages")
async def list_messages(limit: int = 50, offset: int = 0):
    return await messages.list_messages(limit=limit, offset=offset)


@app.get("/messages/{message_id}")
async def get_message(message_id: str):
    msg = await messages.get_message(message_id)
    if not msg:
        raise HTTPException(status_code=404, detail="Message not found")
    return msg


@app.get("/stats")
async def get_stats():
    return await messages.get_stats()


# --- Discovery ---

@app.get("/accounts/discover")
async def discover_settings(email: str):
    return await discover(email)


@app.get("/accounts/discover/stream")
async def discover_settings_stream(email: str):
    return StreamingResponse(
        discover_stream(email),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no"},
    )


# --- Accounts ---

@app.post("/accounts")
async def create_account(data: CreateAccount):
    # Resolve hosts: specific overrides shared
    smtp_host = data.smtp_host or data.host
    imap_host = data.imap_host or data.host

    if not smtp_host or not imap_host:
        raise HTTPException(
            status_code=422,
            detail="Provide 'host' for shared config, or both 'smtp_host' and 'imap_host' separately",
        )

    # Resolve credentials: specific overrides shared
    username = data.username
    password = data.password

    # Need at least shared credentials or both specific sets
    if not username or not password:
        if not (data.smtp_username and data.smtp_password and data.imap_username and data.imap_password):
            raise HTTPException(
                status_code=422,
                detail="Provide 'username'/'password' for shared credentials, or all of 'smtp_username'/'smtp_password'/'imap_username'/'imap_password'",
            )
        # Use smtp credentials as the stored "primary" when no shared creds
        username = data.smtp_username
        password = data.smtp_password

    account = await credential_store.create_account(
        name=data.name,
        smtp_host=smtp_host,
        smtp_port=data.smtp_port,
        imap_host=imap_host,
        imap_port=data.imap_port,
        username=username,
        password=password,
        smtp_username=data.smtp_username,
        smtp_password=data.smtp_password,
        imap_username=data.imap_username,
        imap_password=data.imap_password,
        display_name=data.display_name,
        approval_required=data.approval_required,
        auto_send_threshold=data.auto_send_threshold,
        review_threshold=data.review_threshold,
    )
    return account


@app.get("/accounts")
async def list_accounts():
    return await credential_store.list_accounts()


@app.get("/accounts/{account_id}")
async def get_account(account_id: str):
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    return account


@app.patch("/accounts/{account_id}")
async def update_account(account_id: str, data: UpdateAccount):
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    updates = {}
    if data.display_name is not None:
        updates["display_name"] = data.display_name
    if data.auto_send_threshold is not None:
        updates["auto_send_threshold"] = data.auto_send_threshold
    if data.review_threshold is not None:
        updates["review_threshold"] = data.review_threshold
    if not updates:
        return account
    updated = await credential_store.update_account(account_id, **updates)
    return updated


@app.delete("/accounts/{account_id}")
async def delete_account(request: Request, account_id: str):
    deleted = await credential_store.delete_account(account_id)
    if not deleted:
        raise HTTPException(status_code=404, detail="Account not found")
    request.app.state.smtp_pool.invalidate_account(account_id)
    return {"status": "deleted", "id": account_id}


@app.post("/accounts/{account_id}/verify")
async def verify_account(request: Request, account_id: str):
    account = await credential_store.get_account_with_credentials(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")

    results = {"smtp": None, "imap": None}
    pool = request.app.state.smtp_pool

    # Verify SMTP via pool (acquire validates auth)
    try:
        async with pool.acquire(account) as client:
            await client.noop()
        results["smtp"] = {"status": "ok"}
    except Exception as e:
        results["smtp"] = {"status": "error", "message": str(e)}

    # Update verified timestamp if SMTP succeeded
    if results["smtp"]["status"] == "ok":
        await credential_store.update_verified(account_id)

    return {"id": account_id, "verification": results}


# --- Drafts ---

@app.post("/accounts/{account_id}/drafts", status_code=201)
async def create_draft(account_id: str, data: CreateDraft):
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    draft = await drafts.create_draft(
        account_id=account_id,
        to_addr=data.to,
        subject=data.subject,
        text_content=data.text,
        html_content=data.html,
        in_reply_to=data.in_reply_to,
        metadata=data.metadata,
        created_by=data.created_by,
    )
    return draft


@app.get("/accounts/{account_id}/drafts")
async def list_drafts(
    account_id: str,
    limit: int = 50,
    offset: int = 0,
    status: Optional[str] = None,
    created_by: Optional[str] = None,
    hide_snoozed: bool = False,
):
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    return await drafts.list_drafts(
        account_id, limit=limit, offset=offset, status=status,
        created_by=created_by, hide_snoozed=hide_snoozed,
    )


@app.get("/accounts/{account_id}/drafts/{draft_id}")
async def get_draft(account_id: str, draft_id: str):
    draft = await drafts.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        raise HTTPException(status_code=404, detail="Draft not found")
    return draft


@app.put("/accounts/{account_id}/drafts/{draft_id}")
async def update_draft(account_id: str, draft_id: str, data: UpdateDraft):
    draft = await drafts.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        raise HTTPException(status_code=404, detail="Draft not found")
    if draft["status"] != "draft":
        raise HTTPException(status_code=409, detail=f"Cannot update draft with status '{draft['status']}'")
    fields = {}
    if data.to is not None:
        fields["to_addr"] = data.to
    if data.subject is not None:
        fields["subject"] = data.subject
    if data.text is not None:
        fields["text_content"] = data.text
    if data.html is not None:
        fields["html_content"] = data.html
    if data.in_reply_to is not None:
        fields["in_reply_to"] = data.in_reply_to
    if data.metadata is not None:
        fields["metadata"] = data.metadata
    updated = await drafts.update_draft(draft_id, **fields)
    return updated


@app.patch("/accounts/{account_id}/drafts/{draft_id}")
async def patch_draft(account_id: str, draft_id: str, data: ScheduleDraft):
    draft = await drafts.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        raise HTTPException(status_code=404, detail="Draft not found")
    if draft["status"] != "draft":
        raise HTTPException(status_code=409, detail=f"Cannot update draft with status '{draft['status']}'")
    fields = {}
    for field in data.__fields_set__:
        if field == "send_after":
            fields["send_after"] = data.send_after
        elif field == "snoozed_until":
            fields["snoozed_until"] = data.snoozed_until
    if not fields:
        return draft
    updated = await drafts.update_draft(draft_id, **fields)
    return updated


@app.post("/accounts/{account_id}/drafts/{draft_id}/approve")
async def approve_draft(request: Request, account_id: str, draft_id: str):
    """Send a draft immediately. Records approved_by='review-queue' in metadata."""
    return await _send_draft(request, account_id, draft_id, approved_by="review-queue")


async def _send_draft(
    request: Request,
    account_id: str,
    draft_id: str,
    approved_by: Optional[str] = None,
):
    draft = await drafts.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        raise HTTPException(status_code=404, detail="Draft not found")
    if draft["status"] != "draft":
        raise HTTPException(status_code=409, detail=f"Cannot send draft with status '{draft['status']}'")

    account = await credential_store.get_account_with_credentials(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")

    # Record approval metadata before sending
    if approved_by:
        from datetime import datetime, timezone
        existing_meta = draft.get("metadata") or {}
        existing_meta["approved_at"] = datetime.now(timezone.utc).isoformat()
        existing_meta["approved_by"] = approved_by
        await drafts.update_draft(draft_id, metadata=existing_meta)

    from_addr = account["username"]
    pool = request.app.state.smtp_pool

    # Build MIME message
    msg = build_mime_message(
        from_addr=from_addr,
        to_addr=draft["to_addr"],
        subject=draft["subject"] or "",
        text=draft["text_content"],
        html=draft["html_content"],
        display_name=account.get("display_name"),
    )
    if draft["in_reply_to"]:
        msg["In-Reply-To"] = draft["in_reply_to"]

    # Create message tracking record
    record = await messages.create_message(
        account_id=account_id,
        from_addr=from_addr,
        to_addr=draft["to_addr"],
        subject=draft["subject"],
        text_content=draft["text_content"],
        html_content=draft["html_content"],
    )

    # Send via SMTP
    try:
        smtp_message_id = await send_message(account, msg, pool=pool)
    except SmtpSendError as e:
        await messages.mark_failed(record["id"], e.message)
        return JSONResponse(
            status_code=502,
            content={"error": e.message, "error_type": e.error_type},
        )

    await messages.mark_sent(record["id"], smtp_message_id)
    await drafts.mark_draft_sent(draft_id, record["id"])

    return {
        "status": "sent",
        "draft_id": draft_id,
        "message_id": record["id"],
    }


@app.post("/accounts/{account_id}/drafts/{draft_id}/send")
async def send_draft(
    request: Request,
    account_id: str,
    draft_id: str,
    approved_by: Optional[str] = None,
):
    return await _send_draft(request, account_id, draft_id, approved_by=approved_by)


@app.delete("/accounts/{account_id}/drafts/{draft_id}")
async def discard_draft(account_id: str, draft_id: str):
    draft = await drafts.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        raise HTTPException(status_code=404, detail="Draft not found")
    discarded = await drafts.discard_draft(draft_id)
    if not discarded:
        raise HTTPException(status_code=409, detail=f"Cannot discard draft with status '{draft['status']}'")
    return {"status": "discarded", "id": draft_id}


@app.post("/accounts/{account_id}/drafts/{draft_id}/reject")
async def reject_draft(account_id: str, draft_id: str, data: RejectDraft):
    draft = await drafts.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        raise HTTPException(status_code=404, detail="Draft not found")
    if draft["status"] != "draft":
        raise HTTPException(status_code=409, detail=f"Cannot reject draft with status '{draft['status']}'")

    # Record feedback in metadata before discarding
    from datetime import datetime, timezone
    existing_meta = draft.get("metadata") or {}
    existing_meta["rejected_at"] = datetime.now(timezone.utc).isoformat()
    if data.feedback:
        existing_meta["rejection_feedback"] = data.feedback
    await drafts.update_draft(draft_id, metadata=existing_meta)

    await drafts.discard_draft(draft_id)
    return {"status": "rejected", "id": draft_id}


# --- Review Queue ---

@app.get("/review", response_class=HTMLResponse)
async def review_queue(request: Request):
    return templates.TemplateResponse(request, "review.html")


# --- Inbox ---

@app.get("/accounts/{account_id}/inbox")
async def list_inbox(
    account_id: str,
    folder: str = "INBOX",
    limit: int = 50,
    offset: int = 0,
    q: Optional[str] = None,
):
    account = await credential_store.get_account_with_credentials(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    query = q if q else "ALL"
    try:
        return await search_messages(account, folder=folder, query=query, limit=limit, offset=offset)
    except ImapError as e:
        return JSONResponse(status_code=502, content={"error": e.message, "error_type": e.error_type})


@app.get("/accounts/{account_id}/inbox/{uid}")
async def get_inbox_message(account_id: str, uid: str, folder: str = "INBOX"):
    account = await credential_store.get_account_with_credentials(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    try:
        msg = await fetch_message(account, folder=folder, uid=uid)
    except ImapError as e:
        return JSONResponse(status_code=502, content={"error": e.message, "error_type": e.error_type})
    if not msg:
        raise HTTPException(status_code=404, detail="Message not found")
    return msg


@app.get("/accounts/{account_id}/threads/{message_id}")
async def get_thread_messages(
    account_id: str,
    message_id: str,
    folder: str = "INBOX",
):
    account = await credential_store.get_account_with_credentials(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    try:
        thread = await get_thread(account, message_id=message_id, folder=folder)
    except ImapError as e:
        return JSONResponse(status_code=502, content={"error": e.message, "error_type": e.error_type})
    return {"message_id": message_id, "thread": thread, "count": len(thread)}


@app.get("/accounts/{account_id}/folders")
async def list_account_folders(account_id: str):
    account = await credential_store.get_account_with_credentials(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    try:
        folders = await list_folders(account)
    except ImapError as e:
        return JSONResponse(status_code=502, content={"error": e.message, "error_type": e.error_type})
    return {"folders": folders}


# --- Context (Semantic Search) ---

@app.get("/accounts/{account_id}/context")
async def search_context(
    account_id: str,
    q: str,
    limit: int = 5,
):
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    from app.agent.embeddings import find_similar
    results = await find_similar(account_id, q, limit=limit)
    return {"query": q, "results": results, "count": len(results)}


@app.post("/accounts/{account_id}/embed")
async def bulk_embed(
    account_id: str,
    folder: str = "INBOX",
    limit: int = 500,
):
    account = await credential_store.get_account_with_credentials(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")

    # Fetch recent messages from IMAP
    try:
        msgs = await search_messages(account, folder=folder, limit=limit)
    except ImapError as e:
        return JSONResponse(status_code=502, content={"error": e.message, "error_type": e.error_type})

    # Fetch full bodies for messages that have message_ids
    from app.agent.embeddings import backfill_embeddings
    full_messages = []
    for summary in msgs:
        if not summary.get("message_id"):
            continue
        try:
            full = await fetch_message(account, folder=folder, uid=summary["uid"])
            if full:
                full_messages.append(full)
        except ImapError:
            continue

    result = await backfill_embeddings(account_id, full_messages)
    return {"status": "complete", **result}


# --- Agent ---

@app.get("/agent/status")
async def agent_status(request: Request):
    agent = request.app.state.inbox_agent
    if not agent:
        return {"enabled": False}
    return {"enabled": True, **agent.status()}


@app.get("/agent/actions")
async def agent_actions(limit: int = 50, offset: int = 0):
    from app.db import get_db
    db = await get_db()
    cursor = await db.execute(
        """SELECT id, inbound_message_id, from_addr, subject,
                  classification, confidence, action, reasoning,
                  draft_reply, escalation_note, outbound_message_id, created_at
           FROM agent_actions ORDER BY created_at DESC LIMIT ? OFFSET ?""",
        (limit, offset),
    )
    rows = await cursor.fetchall()
    return [dict(row) for row in rows]


@app.post("/agent/poll")
async def agent_poll(request: Request):
    agent = request.app.state.inbox_agent
    if not agent:
        raise HTTPException(status_code=503, detail="Agent not enabled")
    results = await agent.poll_once()
    return {"polled": True, "actions": results}


if __name__ == "__main__":
    import uvicorn
    uvicorn.run(app, host="0.0.0.0", port=8000)
