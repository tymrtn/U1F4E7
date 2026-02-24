# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)
#
# Demo: run the real inbox agent pipeline against synthetic inbound emails.
# No IMAP connection required — feeds messages directly to _process_message.
#
# Usage:
#   ENVELOPE_SECRET_KEY=demo OPENROUTER_API_KEY=<key> python demo_agent.py

import asyncio
import os
import sys
import json
from datetime import datetime, timezone

# Ensure app is on the path
sys.path.insert(0, os.path.dirname(__file__))

os.environ.setdefault("ENVELOPE_DB_PATH", "envelope.db")

# Strip whitespace/newlines from API key if set via shell env
if "OPENROUTER_API_KEY" in os.environ:
    os.environ["OPENROUTER_API_KEY"] = "".join(os.environ["OPENROUTER_API_KEY"].split())

ACCOUNT_ID = "f7a73b38-fe66-48f4-8a30-bcd664072e4e"

INBOUND_EMAILS = [
    {
        "uid": "demo-001",
        "message_id": "<pricing-001@demo.envelope>",
        "from_addr": "Sophie Laurent <sophie.laurent@example.com>",
        "subject": "Inquiry: Costa Brava villa pricing",
        "date": "Mon, 24 Feb 2026 09:12:00 +0000",
        "text_body": (
            "Hi,\n\n"
            "I saw your listing for the Costa Brava fractional villa. "
            "Could you send me a price breakdown for a 1/8 share? "
            "Also curious about the Q2 2026 availability and what the annual maintenance fee looks like.\n\n"
            "Thanks,\nSophie"
        ),
        "in_reply_to": None,
        "references": None,
    },
    {
        "uid": "demo-002",
        "message_id": "<booking-002@demo.envelope>",
        "from_addr": "Raj Patel <raj.patel@example.com>",
        "subject": "Re: Booking confirmation — Villa Algarve",
        "date": "Mon, 24 Feb 2026 10:30:00 +0000",
        "text_body": (
            "Hello,\n\n"
            "Following up on our call — we'd like to lock in the two-week slot "
            "starting July 5th. Can you confirm whether the 30% deposit can be "
            "paid by bank transfer, and what the exact IBAN is?\n\n"
            "Best,\nRaj"
        ),
        "in_reply_to": "<prev-villa@demo.envelope>",
        "references": "<prev-villa@demo.envelope>",
    },
    {
        "uid": "demo-003",
        "message_id": "<legal-003@demo.envelope>",
        "from_addr": "Elena Vasquez <elena.vasquez@example.com>",
        "subject": "Golden Visa eligibility for property purchase",
        "date": "Mon, 24 Feb 2026 11:05:00 +0000",
        "text_body": (
            "Hi there,\n\n"
            "I'm a US citizen looking at purchasing a fractional share. "
            "Would this qualify for the Spanish Golden Visa program? "
            "My lawyer mentioned the 500k EUR threshold — does a fractional "
            "share count toward that?\n\n"
            "Thanks,\nElena"
        ),
        "in_reply_to": None,
        "references": None,
    },
]


async def main():
    from app.transport.imap import InboundMessage
    from app.agent.inbox_agent import InboxAgent
    from app.db import get_db

    # Minimal account dict — no real credentials needed for classification
    account = {
        "id": ACCOUNT_ID,
        "username": "demo@envelope.ai",
        "display_name": "Envelope Demo",
        "smtp_host": "mail.example.com",
        "smtp_port": 587,
        "imap_host": "mail.example.com",
        "imap_port": 993,
        "password": None,
        "approval_required": True,
        "auto_send_threshold": 0.85,
        "review_threshold": 0.50,
    }

    agent = InboxAgent(smtp_pool=None)

    # Patch out the embeddings lookup for the demo (no indexed mail yet)
    async def _no_semantic(account, msg):
        return ""
    agent._fetch_semantic_context = _no_semantic

    print("\n" + "═" * 62)
    print("  Envelope Inbox Agent — Live Demo")
    print(f"  Model: {os.environ.get('OPENROUTER_MODEL', 'claude-sonnet-4-20250514')}")
    print("═" * 62)

    results = []
    for i, raw in enumerate(INBOUND_EMAILS, 1):
        msg = InboundMessage(
            uid=raw["uid"],
            message_id=raw["message_id"],
            from_addr=raw["from_addr"],
            to_addr="demo@envelope.ai",
            subject=raw["subject"],
            date=raw["date"],
            text_body=raw["text_body"],
            html_body=None,
            in_reply_to=raw["in_reply_to"],
            references=raw["references"],
        )

        print(f"\n[{i}/{len(INBOUND_EMAILS)}] From: {msg.from_addr}")
        print(f"     Subject: {msg.subject}")
        print(f"     Calling LLM...", end="", flush=True)

        # Skip IMAP fetch — call classify directly
        result = await agent._process_message(account, msg)
        results.append(result)

        meta_cursor = await (await __import__('app.db', fromlist=['get_db']).get_db()).execute(
            "SELECT metadata FROM drafts WHERE to_addr = ? ORDER BY created_at DESC LIMIT 1",
            (agent._extract_email(msg.from_addr),)
        )
        row = await meta_cursor.fetchone()
        meta = json.loads(row["metadata"]) if row else {}
        confidence = meta.get("confidence", 0)
        classification = meta.get("classification", result.get("classification", "?"))
        signals = meta.get("signals", {})

        print(f" done")
        print(f"     Classification : {classification}")
        print(f"     Confidence     : {confidence:.2f}")
        chip_parts = []
        if signals.get("kb_match"):
            chip_parts.append("✓ KB match")
        if signals.get("sensitive_categories"):
            chip_parts.append("⚠ " + ", ".join(signals["sensitive_categories"]))
        if signals.get("thread_context"):
            chip_parts.append("✓ thread context")
        if chip_parts:
            print(f"     Signals        : {' | '.join(chip_parts)}")
        print(f"     → Draft created in review queue")

    print("\n" + "═" * 62)
    print(f"  Processed {len(INBOUND_EMAILS)} emails")
    counts = {}
    for r in results:
        c = r.get("classification", "?")
        counts[c] = counts.get(c, 0) + 1
    for k, v in counts.items():
        print(f"    {k}: {v}")
    print("═" * 62)
    print(f"\n  Review queue: http://localhost:8000/review\n")


if __name__ == "__main__":
    asyncio.run(main())
