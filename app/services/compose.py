# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import base64
import logging
import os
from datetime import datetime, timedelta, timezone
from typing import Optional

from app import drafts, messages
from app.credentials import store as credential_store
from app.services.actions import log_action
from app.services import scoring as scoring_svc
from app.transport.imap import save_to_drafts
from app.transport.smtp import SmtpSendError, build_mime_message, send_message

EDITABLE_DRAFT_STATUSES = {"draft", "pending_review", "blocked"}
HUMAN_APPROVAL_SOURCES = {"review-queue", "server-routing"}


class DraftRoutingError(Exception):
    def __init__(
        self,
        status_code: int,
        detail: str,
        error_type: str | None = None,
    ):
        super().__init__(detail)
        self.status_code = status_code
        self.detail = detail
        self.error_type = error_type


def _attachments_meta(attachments: Optional[list[dict]]) -> Optional[list[dict]]:
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


def _apply_signature(
    account: dict,
    text_body: Optional[str],
    html_body: Optional[str],
) -> tuple[Optional[str], Optional[str]]:
    sig_text = account.get("signature_text")
    sig_html = account.get("signature_html")
    if sig_text and text_body:
        text_body = text_body + "\n\n-- \n" + sig_text
    if sig_html and html_body:
        if "</body>" in html_body:
            html_body = html_body.replace(
                "</body>",
                f'<div class="env-signature">{sig_html}</div></body>',
                1,
            )
        else:
            html_body = html_body + f'<div class="env-signature">{sig_html}</div>'
    return text_body, html_body


def resolve_routing_status(
    confidence: float, account: dict
) -> tuple[str, str | None]:
    """Returns (routing_status, send_after_iso_or_none).
    Zones: blocked -> pending_review -> delayed -> sent
    """
    auto_send = account.get("auto_send_threshold", 0.85)
    review = account.get("review_threshold", 0.50)
    delay_margin = account.get("delay_margin", 0.10)
    delay_minutes = account.get("delay_minutes", 15)

    if confidence >= auto_send:
        return "sent", None
    gap = auto_send - delay_margin
    if confidence >= gap:
        send_after = (
            datetime.now(timezone.utc) + timedelta(minutes=delay_minutes)
        ).isoformat()
        return "delayed", send_after
    if confidence >= review:
        return "pending_review", None
    return "blocked", None


async def send_draft(  # noqa: PLR0913
    account_id: str,
    draft_id: str,
    smtp_pool=None,
    approved_by: Optional[str] = None,
) -> dict:
    draft = await drafts.get_draft(draft_id)
    if not draft or draft["account_id"] != account_id:
        raise DraftRoutingError(status_code=404, detail="Draft not found")

    if draft["status"] not in ("draft",):
        if draft["status"] in ("pending_review", "blocked"):
            if approved_by not in HUMAN_APPROVAL_SOURCES:
                raise DraftRoutingError(
                    status_code=403,
                    detail=(
                        f"Cannot send draft with status '{draft['status']}'. "
                        "Pending and blocked drafts are saved to your IMAP Drafts folder — "
                        "open your email client to review and send."
                    ),
                    error_type="agent_approval_denied",
                )
        elif draft["status"] not in EDITABLE_DRAFT_STATUSES:
            raise DraftRoutingError(
                status_code=409,
                detail=f"Cannot send draft with status '{draft['status']}'",
            )

    account = await credential_store.get_account_with_credentials(account_id)
    if not account:
        raise DraftRoutingError(status_code=404, detail="Account not found")

    if approved_by:
        existing_meta = draft.get("metadata") or {}
        existing_meta["approved_at"] = datetime.now(timezone.utc).isoformat()
        existing_meta["approved_by"] = approved_by
        draft = await drafts.update_draft(draft_id, metadata=existing_meta)

    from_addr = account["username"]
    draft_attachments = draft.get("attachments") or []
    text_body, html_body = _apply_signature(
        account,
        draft["text_content"],
        draft["html_content"],
    )
    msg = build_mime_message(
        from_addr=from_addr,
        to_addr=draft["to_addr"],
        subject=draft["subject"] or "",
        text=text_body,
        html=html_body,
        display_name=account.get("display_name"),
        cc=draft.get("cc_addr"),
        bcc=draft.get("bcc_addr"),
        reply_to=draft.get("reply_to"),
        attachments=draft_attachments or None,
    )
    if draft["in_reply_to"]:
        msg["In-Reply-To"] = draft["in_reply_to"]
        draft_meta = draft.get("metadata") or {}
        msg["References"] = draft_meta.get("references") or draft["in_reply_to"]

    record = await messages.create_message(
        account_id=account_id,
        from_addr=from_addr,
        to_addr=draft["to_addr"],
        subject=draft["subject"],
        text_content=text_body,
        html_content=html_body,
        attachments_meta=_attachments_meta(draft_attachments),
    )

    try:
        smtp_message_id = await send_message(account, msg, pool=smtp_pool)
    except SmtpSendError as exc:
        await messages.mark_failed(record["id"], exc.message)
        raise DraftRoutingError(
            status_code=502,
            detail=exc.message,
            error_type=exc.error_type,
        ) from exc

    await messages.mark_sent(record["id"], smtp_message_id)
    await drafts.mark_draft_sent(draft_id, record["id"])
    return {
        "status": "sent",
        "draft_id": draft_id,
        "message_id": record["id"],
    }


async def route_composed_email(  # noqa: PLR0913
    account_id: str,
    to_addr: str,
    justification: str,
    confidence: Optional[float] = None,
    attribution: Optional[dict] = None,
    subject: Optional[str] = None,
    text_content: Optional[str] = None,
    html_content: Optional[str] = None,
    in_reply_to: Optional[str] = None,
    metadata: Optional[dict] = None,
    created_by: Optional[str] = "agent",
    send_after: Optional[str] = None,
    snoozed_until: Optional[str] = None,
    cc_addr: Optional[str] = None,
    bcc_addr: Optional[str] = None,
    reply_to: Optional[str] = None,
    attachments: Optional[list[dict]] = None,
    smtp_pool=None,
) -> dict:
    account = await credential_store.get_account(account_id)
    if not account:
        raise DraftRoutingError(status_code=404, detail="Account not found")

    score = confidence
    attribution_applied: dict[str, float] = {}

    if attribution is not None:
        # New format: flat attribute list
        if "attributes" in attribution and isinstance(attribution["attributes"], list):
            _domain, base_score, catalog = await scoring_svc.get_attribute_catalog(account_id)
            score, attribution_applied = scoring_svc.compute_attribute_score(
                attribution["attributes"], catalog, base_score
            )
        else:
            # Legacy format: dimension-based rubric
            rubric = await scoring_svc.get_scoring_rubric(account_id)
            score, attribution_applied = scoring_svc.compute_attribution_score(
                attribution,
                rubric,
            )

    if score is None:
        raise DraftRoutingError(
            status_code=422,
            detail="Either confidence or attribution is required",
        )

    routing_status, delayed_send_after = resolve_routing_status(score, account)
    auto_send_threshold = account.get("auto_send_threshold", 0.85)
    review_threshold = account.get("review_threshold", 0.50)
    routing_metadata = dict(metadata or {})
    routing_metadata.update({
        "confidence": score,
        "computed_score": score,
        "justification": justification,
        "routing_status": routing_status,
        "routed_at": datetime.now(timezone.utc).isoformat(),
        "auto_send_threshold": auto_send_threshold,
        "review_threshold": review_threshold,
    })
    if attribution is not None:
        routing_metadata["attribution"] = attribution
        routing_metadata["attribution_applied"] = attribution_applied

    # For delayed zone, set send_after on the draft (reuses DraftScheduler)
    effective_send_after = send_after
    if routing_status == "delayed" and delayed_send_after:
        effective_send_after = delayed_send_after

    draft_status = "draft" if routing_status in ("sent", "delayed") else routing_status
    draft = await drafts.create_draft(
        account_id=account_id,
        to_addr=to_addr,
        status=draft_status,
        subject=subject,
        text_content=text_content,
        html_content=html_content,
        in_reply_to=in_reply_to,
        metadata=routing_metadata,
        created_by=created_by,
        send_after=effective_send_after,
        snoozed_until=snoozed_until,
        cc_addr=cc_addr,
        bcc_addr=bcc_addr,
        reply_to=reply_to,
        attachments=attachments,
    )

    await log_action(
        account_id=account_id,
        action_type="send_decision",
        confidence=score,
        justification=justification,
        action_taken=(
            f"Routed compose_email to {routing_status} "
            f"(review_threshold={review_threshold:.2f}, "
            f"auto_send_threshold={auto_send_threshold:.2f})"
        ),
        draft_id=draft["id"],
    )

    # Save to IMAP Drafts for pending_review and blocked
    if routing_status in ("pending_review", "blocked"):
        account_creds = await credential_store.get_account_with_credentials(account_id)
        if account_creds:
            text_body, html_body = _apply_signature(
                account_creds,
                text_content,
                html_content,
            )
            mime_msg = build_mime_message(
                from_addr=account_creds["username"],
                to_addr=to_addr,
                subject=subject or "",
                text=text_body,
                html=html_body,
                display_name=account_creds.get("display_name"),
                cc=cc_addr,
                bcc=bcc_addr,
                reply_to=reply_to,
                attachments=attachments,
            )
            if in_reply_to:
                mime_msg["In-Reply-To"] = in_reply_to
                mime_msg["References"] = routing_metadata.get("references") or in_reply_to
            mime_msg["X-Envelope-Draft-Id"] = draft["id"]
            # Only save to IMAP Drafts if the email is NOT auto-sending.
            # Auto-sent emails go via SMTP — no draft needed.
            if routing_status != "sent":
                try:
                    await save_to_drafts(account_creds, mime_msg)
                except Exception as exc:
                    logging.getLogger(__name__).error(
                        "Failed to save draft %s to IMAP Drafts folder: %s",
                        draft["id"], exc,
                        exc_info=True,
                    )

    if routing_status == "sent":
        await send_draft(
            account_id=account_id,
            draft_id=draft["id"],
            smtp_pool=smtp_pool,
            approved_by="server-routing",
        )

    base_url = os.getenv("ENVELOPE_BASE_URL", "")
    review_url = f"{base_url}/review?highlight={draft['id']}"

    # Score redacted from response to prevent agents from reverse-engineering
    # the scoring rubric via probing. computed_score and attribution_applied
    # are stored in draft metadata for admin/internal use only.
    return {
        "draft_id": draft["id"],
        "status": routing_status,
        "routing_status": routing_status,
        "to": to_addr,
        "subject": subject,
        "review_url": review_url,
        "send_after": effective_send_after,
    }
