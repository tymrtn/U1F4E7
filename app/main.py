# Envelope Email - Transactional Email API

import base64
from contextlib import asynccontextmanager
from fastapi import FastAPI, Request, HTTPException, Depends, Path
from fastapi.security import HTTPBearer, HTTPAuthorizationCredentials
from fastapi.responses import HTMLResponse
from fastapi.staticfiles import StaticFiles
from fastapi.templating import Jinja2Templates
from fastapi.middleware.cors import CORSMiddleware
from pydantic import BaseModel
from dotenv import load_dotenv
import os
from typing import Optional, Literal
from urllib.parse import unquote

import uuid
from fastapi.responses import JSONResponse, Response
from starlette.responses import StreamingResponse

from datetime import datetime, timezone, timedelta

from app.db import init_db, close_db, get_db
from app.credentials import store as credential_store
from app.transport.smtp import build_mime_message, send_message, SmtpSendError
from app.transport.pool import SmtpConnectionPool
from app.transport.worker import SendWorker
from app.workers.draft_scheduler import DraftScheduler
from app.transport.imap import search_messages, fetch_message, list_folders, get_thread, ImapError
from app.transport.webhook import WebhookPoller
from app import messages
from app import drafts
from app.discovery import discover, discover_stream
from app.services import policy as policy_svc
from app.services import actions as actions_svc
from app.services import compose as compose_svc
from app.services.start_here import build_start_here_response

load_dotenv()

# --- API Key Auth ---

_bearer = HTTPBearer(auto_error=False)
_api_key = os.getenv("ENVELOPE_API_KEY")

_PUBLIC_PATHS = {"/health", "/", "/review", "/openapi.json", "/docs", "/redoc"}


async def require_api_key(
    request: Request,
    creds: HTTPAuthorizationCredentials | None = Depends(_bearer),
):
    """Reject requests without a valid Bearer token.
    Disabled when ENVELOPE_API_KEY is not set (local dev).
    Public paths and /static are always exempt."""
    if not _api_key:
        return
    if request.url.path in _PUBLIC_PATHS or request.url.path.startswith("/static"):
        return
    if request.url.path.endswith("/start-here"):
        return
    if request.url.path.startswith("/track/"):
        return
    if not creds or creds.credentials != _api_key:
        raise HTTPException(status_code=401, detail="Invalid or missing API key")


@asynccontextmanager
async def lifespan(app: FastAPI):
    await init_db()
    app.state.smtp_pool = SmtpConnectionPool()
    app.state.smtp_pool.start_cleanup_task()
    app.state.send_worker = SendWorker(app.state.smtp_pool)
    await app.state.send_worker.start()
    app.state.draft_scheduler = DraftScheduler(app.state.smtp_pool)
    await app.state.draft_scheduler.start()
    app.state.webhook_poller = WebhookPoller()
    await app.state.webhook_poller.start()

    yield

    await app.state.send_worker.stop()
    await app.state.draft_scheduler.stop()
    await app.state.webhook_poller.stop()
    await app.state.smtp_pool.close_all()
    await close_db()

app = FastAPI(
    title="Envelope Email API",
    version="0.3.0",
    lifespan=lifespan,
    dependencies=[Depends(require_api_key)],
)

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

MAX_ATTACHMENTS_BYTES = 40 * 1024 * 1024  # 40 MB


class Attachment(BaseModel):
    filename: str
    content: str  # base64-encoded
    content_type: Optional[str] = None
    content_id: Optional[str] = None


class SendEmail(BaseModel):
    account_id: str
    to: str
    subject: str
    from_email: Optional[str] = None
    html: Optional[str] = None
    text: Optional[str] = None
    display_name: Optional[str] = None
    cc: Optional[str] = None
    bcc: Optional[str] = None
    reply_to: Optional[str] = None
    headers: Optional[dict] = None
    use_signature: bool = True
    track_opens: bool = False
    wait: bool = True
    attachments: Optional[list[Attachment]] = None


class CreateDraft(BaseModel):
    to: str
    subject: Optional[str] = None
    text: Optional[str] = None
    html: Optional[str] = None
    in_reply_to: Optional[str] = None
    metadata: Optional[dict] = None
    created_by: Optional[str] = None
    send_after: Optional[str] = None
    cc: Optional[str] = None
    bcc: Optional[str] = None
    reply_to: Optional[str] = None
    attachments: Optional[list[Attachment]] = None
    confidence: Optional[float] = None
    justification: Optional[str] = None


class UpdateDraft(BaseModel):
    to: Optional[str] = None
    subject: Optional[str] = None
    text: Optional[str] = None
    html: Optional[str] = None
    in_reply_to: Optional[str] = None
    metadata: Optional[dict] = None
    cc: Optional[str] = None
    bcc: Optional[str] = None
    reply_to: Optional[str] = None
    attachments: Optional[list[Attachment]] = None


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
    rate_limit_per_hour: Optional[int] = None


class UpdateAccount(BaseModel):
    display_name: Optional[str] = None
    auto_send_threshold: Optional[float] = None
    review_threshold: Optional[float] = None
    rate_limit_per_hour: Optional[int] = None
    webhook_url: Optional[str] = None
    webhook_secret: Optional[str] = None
    signature_text: Optional[str] = None
    signature_html: Optional[str] = None
    smtp_host: Optional[str] = None
    smtp_port: Optional[int] = None
    imap_host: Optional[str] = None
    imap_port: Optional[int] = None


class DomainPolicyIn(BaseModel):
    name: str
    description: Optional[str] = None
    values: Optional[list[str]] = None
    tone: Optional[str] = None
    style: Optional[str] = None
    kb_text: Optional[str] = None


class AddressPolicyIn(BaseModel):
    pattern: str
    purpose: Optional[str] = None
    reply_instructions: Optional[str] = None
    escalation_rules: Optional[str] = None
    routing_rules: Optional[str] = None
    trash_criteria: Optional[str] = None
    help_resources: Optional[str] = None
    sensitive_topics: Optional[list[str]] = None
    confidence_threshold: float = 0.7
    webhook_url: Optional[str] = None


class LogActionIn(BaseModel):
    account_id: str
    action_type: Literal["inbound_route", "draft_approve", "draft_reject", "send_decision", "escalate", "trash"]
    confidence: float
    justification: str
    action_taken: str
    message_id: Optional[str] = None
    draft_id: Optional[str] = None


# --- Attachment helpers ---

def _validate_attachments(attachments: Optional[list[Attachment]]) -> Optional[list[dict]]:
    """Validate size and return serializable dicts. Returns None if no attachments."""
    if not attachments:
        return None
    total_bytes = 0
    result = []
    for att in attachments:
        decoded = base64.b64decode(att.content)
        total_bytes += len(decoded)
        result.append({
            "filename": att.filename,
            "content": att.content,
            "content_type": att.content_type,
            "content_id": att.content_id,
        })
    if total_bytes > MAX_ATTACHMENTS_BYTES:
        raise HTTPException(
            status_code=422,
            detail=f"Total attachment size ({total_bytes} bytes) exceeds 40 MB limit",
        )
    return result


def _attachments_meta(attachments: Optional[list[dict]]) -> Optional[list[dict]]:
    """Build metadata-only list (no binary content) for audit trail."""
    if not attachments:
        return None
    meta = []
    for att in attachments:
        decoded = base64.b64decode(att["content"])
        meta.append({
            "filename": att["filename"],
            "content_type": att.get("content_type"),
            "size_bytes": len(decoded),
        })
    return meta


# --- Rate limit helper ---

async def _check_rate_limit(account_id: str, limit: Optional[int]) -> bool:
    if not limit:
        return True
    db = await get_db()
    window_start = (datetime.now(timezone.utc) - timedelta(hours=1)).isoformat()
    cursor = await db.execute(
        "SELECT COUNT(*) FROM messages WHERE account_id = ? AND created_at > ? AND status != 'failed'",
        (account_id, window_start),
    )
    row = await cursor.fetchone()
    return row[0] < limit


# --- Health ---

@app.get("/health")
async def health():
    from app.db import get_db
    db = await get_db()
    cursor = await db.execute("SELECT COUNT(*) FROM accounts")
    row = await cursor.fetchone()
    return {"status": "ok", "version": app.version, "accounts": row[0]}


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

    if not await _check_rate_limit(data.account_id, account.get("rate_limit_per_hour")):
        limit = account.get("rate_limit_per_hour")
        return JSONResponse(
            status_code=429,
            content={"error": "rate_limit_exceeded", "limit": limit},
        )

    att_dicts = _validate_attachments(data.attachments)
    att_meta = _attachments_meta(att_dicts)

    from_addr = data.from_email or account["username"]
    pool = request.app.state.smtp_pool

    # Inject account signature
    text_body = data.text
    html_body = data.html
    if data.use_signature:
        sig_text = account.get("signature_text")
        sig_html = account.get("signature_html")
        if sig_text and text_body:
            text_body = text_body + "\n\n-- \n" + sig_text
        if sig_html and html_body:
            if "</body>" in html_body:
                html_body = html_body.replace("</body>", f'<div class="env-signature">{sig_html}</div></body>', 1)
            else:
                html_body = html_body + f'<div class="env-signature">{sig_html}</div>'

    # Prepare open tracking token
    tracking_token = None
    if data.track_opens and html_body:
        tracking_token = str(uuid.uuid4())
        base_url = os.getenv("ENVELOPE_BASE_URL", "")
        pixel = f'<img src="{base_url}/track/{tracking_token}" width="1" height="1" style="display:none;" />'
        if "</body>" in html_body:
            html_body = html_body.replace("</body>", pixel + "</body>", 1)
        else:
            html_body = html_body + pixel

    # Async mode: queue and return immediately
    if not data.wait:
        # Create as 'queued' so the background worker picks it up
        record = await messages.create_message(
            account_id=data.account_id,
            from_addr=from_addr,
            to_addr=data.to,
            subject=data.subject,
            text_content=text_body,
            html_content=html_body,
            initial_status="queued",
            track_opens=data.track_opens,
            tracking_token=tracking_token,
            attachments_meta=att_meta,
        )
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
    # Create as 'sending' so the background worker never picks it up (prevents duplicate sends)
    record = await messages.create_message(
        account_id=data.account_id,
        from_addr=from_addr,
        to_addr=data.to,
        subject=data.subject,
        text_content=text_body,
        html_content=html_body,
        initial_status="sending",
        track_opens=data.track_opens,
        tracking_token=tracking_token,
        attachments_meta=att_meta,
    )

    msg = build_mime_message(
        from_addr=from_addr,
        to_addr=data.to,
        subject=data.subject,
        text=text_body,
        html=html_body,
        display_name=data.display_name or account.get("display_name"),
        cc=data.cc,
        bcc=data.bcc,
        reply_to=data.reply_to,
        custom_headers=data.headers,
        attachments=att_dicts,
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


# 1x1 transparent GIF bytes
_TRANSPARENT_GIF = (
    b"\x47\x49\x46\x38\x39\x61\x01\x00\x01\x00\x80\x00\x00"
    b"\xff\xff\xff\x00\x00\x00\x21\xf9\x04\x00\x00\x00\x00"
    b"\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02"
    b"\x44\x01\x00\x3b"
)


@app.get("/track/{token}")
async def track_open(request: Request, token: str):
    """Open tracking pixel. Always returns a 1x1 GIF, never 404."""
    user_agent = request.headers.get("user-agent")
    ip_addr = request.client.host if request.client else None
    await messages.record_open(token, user_agent=user_agent, ip_addr=ip_addr)
    return Response(content=_TRANSPARENT_GIF, media_type="image/gif")


@app.get("/messages/{message_id}/opens")
async def list_message_opens(message_id: str):
    msg = await messages.get_message(message_id)
    if not msg:
        raise HTTPException(status_code=404, detail="Message not found")
    return await messages.list_opens(message_id)


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
        rate_limit_per_hour=data.rate_limit_per_hour,
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
    if data.rate_limit_per_hour is not None:
        updates["rate_limit_per_hour"] = data.rate_limit_per_hour
    if data.webhook_url is not None:
        updates["webhook_url"] = data.webhook_url
    if data.webhook_secret is not None:
        updates["webhook_secret"] = data.webhook_secret
    if data.signature_text is not None:
        updates["signature_text"] = data.signature_text
    if data.signature_html is not None:
        updates["signature_html"] = data.signature_html
    if data.smtp_host is not None:
        updates["smtp_host"] = data.smtp_host
    if data.smtp_port is not None:
        updates["smtp_port"] = data.smtp_port
    if data.imap_host is not None:
        updates["imap_host"] = data.imap_host
    if data.imap_port is not None:
        updates["imap_port"] = data.imap_port
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
async def create_draft(request: Request, account_id: str, data: CreateDraft):
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    att_dicts = _validate_attachments(data.attachments) if data.attachments else None
    if data.confidence is not None:
        return await compose_svc.route_composed_email(
            account_id=account_id,
            to_addr=data.to,
            confidence=data.confidence,
            justification=data.justification or "Draft created via REST compose routing",
            subject=data.subject,
            text_content=data.text,
            html_content=data.html,
            in_reply_to=data.in_reply_to,
            metadata=data.metadata,
            created_by=data.created_by or "agent",
            send_after=data.send_after,
            cc_addr=data.cc,
            bcc_addr=data.bcc,
            reply_to=data.reply_to,
            attachments=att_dicts,
            smtp_pool=request.app.state.smtp_pool,
        )
    draft = await drafts.create_draft(
        account_id=account_id,
        to_addr=data.to,
        subject=data.subject,
        text_content=data.text,
        html_content=data.html,
        in_reply_to=data.in_reply_to,
        metadata=data.metadata,
        created_by=data.created_by,
        send_after=data.send_after,
        cc_addr=data.cc,
        bcc_addr=data.bcc,
        reply_to=data.reply_to,
        attachments=att_dicts,
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
    if draft["status"] not in compose_svc.EDITABLE_DRAFT_STATUSES:
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
    if data.cc is not None:
        fields["cc_addr"] = data.cc
    if data.bcc is not None:
        fields["bcc_addr"] = data.bcc
    if data.reply_to is not None:
        fields["reply_to"] = data.reply_to
    if data.attachments is not None:
        fields["attachments"] = _validate_attachments(data.attachments)
    updated = await drafts.update_draft(draft_id, **fields)
    return updated


@app.patch("/accounts/{account_id}/drafts/{draft_id}")
async def patch_draft(account_id: str, draft_id: str, data: ScheduleDraft):
    draft = await drafts.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        raise HTTPException(status_code=404, detail="Draft not found")
    if draft["status"] not in compose_svc.EDITABLE_DRAFT_STATUSES:
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
    try:
        return await compose_svc.send_draft(
            account_id=account_id,
            draft_id=draft_id,
            smtp_pool=request.app.state.smtp_pool,
            approved_by=approved_by,
        )
    except compose_svc.DraftRoutingError as exc:
        if exc.status_code == 502:
            return JSONResponse(
                status_code=502,
                content={"error": exc.detail, "error_type": exc.error_type},
            )
        raise HTTPException(status_code=exc.status_code, detail=exc.detail)


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
    if draft["status"] not in compose_svc.EDITABLE_DRAFT_STATUSES:
        raise HTTPException(status_code=409, detail=f"Cannot reject draft with status '{draft['status']}'")

    # Record feedback in metadata before discarding
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
    from app.embeddings import find_similar
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
    from app.embeddings import backfill_embeddings
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



# --- Domain Policy (Story 011) ---

@app.post("/accounts/{account_id}/domain-policy")
async def upsert_domain_policy(account_id: str, data: DomainPolicyIn):
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    policy = await policy_svc.upsert_domain_policy(
        account_id,
        name=data.name,
        description=data.description,
        values=data.values,
        tone=data.tone,
        style=data.style,
        kb_text=data.kb_text,
    )
    return policy


@app.get("/accounts/{account_id}/domain-policy")
async def get_domain_policy(account_id: str):
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    policy = await policy_svc.get_domain_policy(account_id)
    if not policy:
        raise HTTPException(status_code=404, detail="Domain policy not found")
    kb_text = policy.get("kb_text") or ""
    if len(kb_text) > 2000:
        policy = dict(policy)
        policy["kb_text"] = None
        policy["kb_text_truncated"] = True
        policy["kb_text_url"] = f"/accounts/{account_id}/domain-policy"
    return policy


@app.post("/accounts/{account_id}/address-policies", status_code=201)
async def create_address_policy(account_id: str, data: AddressPolicyIn):
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    policy = await policy_svc.upsert_address_policy(
        account_id,
        data.pattern,
        purpose=data.purpose,
        reply_instructions=data.reply_instructions,
        escalation_rules=data.escalation_rules,
        routing_rules=data.routing_rules,
        trash_criteria=data.trash_criteria,
        help_resources=data.help_resources,
        sensitive_topics=data.sensitive_topics,
        confidence_threshold=data.confidence_threshold,
        webhook_url=data.webhook_url,
    )
    return policy


@app.get("/accounts/{account_id}/address-policies")
async def list_address_policies(account_id: str):
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    return await policy_svc.list_address_policies(account_id)


@app.get("/accounts/{account_id}/address-policies/{pattern}")
async def get_address_policy(account_id: str, pattern: str = Path(...)):
    pattern = unquote(pattern)
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    policy = await policy_svc.get_address_policy(account_id, pattern)
    if not policy:
        raise HTTPException(status_code=404, detail="Address policy not found")
    return policy


@app.put("/accounts/{account_id}/address-policies/{pattern}")
async def update_address_policy(account_id: str, pattern: str, data: AddressPolicyIn):
    pattern = unquote(pattern)
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    policy = await policy_svc.upsert_address_policy(
        account_id,
        pattern,
        purpose=data.purpose,
        reply_instructions=data.reply_instructions,
        escalation_rules=data.escalation_rules,
        routing_rules=data.routing_rules,
        trash_criteria=data.trash_criteria,
        help_resources=data.help_resources,
        sensitive_topics=data.sensitive_topics,
        confidence_threshold=data.confidence_threshold,
        webhook_url=data.webhook_url,
    )
    return policy


@app.delete("/accounts/{account_id}/address-policies/{pattern}")
async def delete_address_policy(account_id: str, pattern: str = Path(...)):
    pattern = unquote(pattern)
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    deleted = await policy_svc.delete_address_policy(account_id, pattern)
    if not deleted:
        raise HTTPException(status_code=404, detail="Address policy not found")
    return {"status": "deleted", "pattern": pattern}


# --- Start Here (Story 012) ---

@app.get("/accounts/{account_id}/start-here")
async def start_here(account_id: str):
    return await build_start_here_response(account_id)


# --- Action Log (Story 013) ---

@app.post("/actions/log", status_code=201)
async def log_action(data: LogActionIn):
    entry = await actions_svc.log_action(
        account_id=data.account_id,
        action_type=data.action_type,
        confidence=data.confidence,
        justification=data.justification,
        action_taken=data.action_taken,
        message_id=data.message_id,
        draft_id=data.draft_id,
    )
    return entry


@app.get("/accounts/{account_id}/actions")
async def list_actions(
    account_id: str,
    limit: int = 50,
    offset: int = 0,
    draft_id: Optional[str] = None,
    message_id: Optional[str] = None,
):
    account = await credential_store.get_account(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")
    return await actions_svc.list_actions(
        account_id, limit=limit, offset=offset,
        draft_id=draft_id, message_id=message_id,
    )


@app.get("/actions/log/{log_id}")
async def get_action(log_id: str):
    entry = await actions_svc.get_action(log_id)
    if not entry:
        raise HTTPException(status_code=404, detail="Action log entry not found")
    return entry


# --- MCP Server (Story 014) ---

try:
    from app.mcp import mcp
    app.mount("/mcp", mcp.sse_app())
except ImportError:
    pass


if __name__ == "__main__":
    import uvicorn
    uvicorn.run(app, host="0.0.0.0", port=8000)
