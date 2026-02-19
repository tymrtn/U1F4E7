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

import aiosmtplib
from fastapi.responses import JSONResponse

from app.db import init_db
from app.credentials import store as credential_store
from app.transport.smtp import build_mime_message, send_message, SmtpSendError

load_dotenv()


@asynccontextmanager
async def lifespan(app: FastAPI):
    await init_db()
    yield

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
async def send_email(data: SendEmail):
    account = await credential_store.get_account_with_credentials(data.account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")

    from_addr = data.from_email or account["username"]

    msg = build_mime_message(
        from_addr=from_addr,
        to_addr=data.to,
        subject=data.subject,
        text=data.text,
        html=data.html,
        display_name=account.get("display_name"),
    )

    try:
        message_id = await send_message(account, msg)
    except SmtpSendError as e:
        return JSONResponse(
            status_code=502,
            content={"error": e.message, "error_type": e.error_type},
        )

    return {
        "status": "sent",
        "message_id": message_id,
        "envelope": {
            "from": from_addr,
            "to": data.to,
            "subject": data.subject,
        },
    }


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
async def delete_account(account_id: str):
    deleted = await credential_store.delete_account(account_id)
    if not deleted:
        raise HTTPException(status_code=404, detail="Account not found")
    return {"status": "deleted", "id": account_id}


@app.post("/accounts/{account_id}/verify")
async def verify_account(account_id: str):
    account = await credential_store.get_account_with_credentials(account_id)
    if not account:
        raise HTTPException(status_code=404, detail="Account not found")

    results = {"smtp": None, "imap": None}

    # Verify SMTP
    try:
        smtp = aiosmtplib.SMTP(
            hostname=account["smtp_host"],
            port=account["smtp_port"],
            use_tls=account["smtp_port"] == 465,
            start_tls=account["smtp_port"] != 465,
        )
        await smtp.connect()
        await smtp.login(
            account["effective_smtp_username"],
            account["effective_smtp_password"],
        )
        await smtp.quit()
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
