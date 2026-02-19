# Envelope Email - Transactional Email API (Mailgun Competitor)
# Aposema Licensed Code
# Copyright (c) 2026 Tyler Martin (aposema.com)
# Licensed under Aposema Protocol: infer|public

from fastapi import FastAPI, Request, Form, Depends
from fastapi.responses import HTMLResponse
from fastapi.staticfiles import StaticFiles
from fastapi.templating import Jinja2Templates
from fastapi.middleware.cors import CORSMiddleware
from pydantic import BaseModel
import resend
from dotenv import load_dotenv
import os
from typing import Optional

load_dotenv()

app = FastAPI(title="Envelope Email API", version="0.1.0")

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

# Resend config
resend.api_key = os.getenv("RESEND_API_KEY")

class SendEmail(BaseModel):
    from_email: str
    to: str
    subject: str
    html: Optional[str] = None
    text: Optional[str] = None

@app.get("/", response_class=HTMLResponse)
async def dashboard(request: Request):
    return templates.TemplateResponse("index.html", {"request": request})

@app.post("/send")
async def send_email(data: SendEmail):
    try:
        params = {
            'from': data.from_email,
            'to': [data.to],
            'subject': data.subject,
        }
        if data.html:
            params['html'] = data.html
        if data.text:
            params['text'] = data.text
        
        email = resend.Emails.send(params)
        return {"status": "success", "id": email["id"]}
    except Exception as e:
        return {"status": "error", "message": str(e)}

@app.post("/webhooks/resend")
async def resend_webhook(request: Request):
    # Handle Resend webhooks (events: delivered, bounced, etc.)
    body = await request.body()
    # TODO: Log to DB or file
    print("Resend webhook:", body)
    return {"status": "received"}

@app.get("/docs")
async def docs():
    return {"docs": "Open http://localhost:8000/docs for interactive API docs"}

if __name__ == "__main__":
    import uvicorn
    uvicorn.run(app, host="0.0.0.0", port=8000)
