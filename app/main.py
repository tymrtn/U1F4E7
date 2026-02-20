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
from app import messages
from app.discovery import discover, discover_stream

load_dotenv()


@asynccontextmanager
async def lifespan(app: FastAPI):
    await init_db()
    app.state.smtp_pool = SmtpConnectionPool()
    app.state.smtp_pool.start_cleanup_task()
    app.state.send_worker = SendWorker(app.state.smtp_pool)
    await app.state.send_worker.start()
    yield
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


# --- Dashboard ---

@app.get("/", response_class=HTMLResponse)
async def dashboard(request: Request):
    return templates.TemplateResponse("index.html", {"request": request})


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


if __name__ == "__main__":
    import uvicorn
    uvicorn.run(app, host="0.0.0.0", port=8000)
