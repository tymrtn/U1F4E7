import asyncio
from email.message import EmailMessage
from typing import Optional

import aiosmtplib
from aiosmtplib.errors import (
    SMTPAuthenticationError,
    SMTPRecipientsRefused,
    SMTPConnectError,
)


class SmtpSendError(Exception):
    def __init__(self, error_type: str, message: str):
        self.error_type = error_type
        self.message = message
        super().__init__(message)


def build_mime_message(
    from_addr: str,
    to_addr: str,
    subject: str,
    text: Optional[str] = None,
    html: Optional[str] = None,
    display_name: Optional[str] = None,
) -> EmailMessage:
    msg = EmailMessage()

    if display_name:
        msg["From"] = f"{display_name} <{from_addr}>"
    else:
        msg["From"] = from_addr
    msg["To"] = to_addr
    msg["Subject"] = subject

    if text and html:
        msg.set_content(text)
        msg.add_alternative(html, subtype="html")
    elif html:
        msg.set_content(html, subtype="html")
    elif text:
        msg.set_content(text)
    else:
        msg.set_content("")

    return msg


async def send_message(account: dict, msg: EmailMessage, pool=None) -> str:
    """Send an EmailMessage via SMTP. Uses pool if provided, otherwise direct."""
    try:
        if pool is not None:
            async with pool.acquire(account) as client:
                response = await client.send_message(msg)
                return msg["Message-ID"] or response[1]
        else:
            port = account["smtp_port"]
            smtp = aiosmtplib.SMTP(
                hostname=account["smtp_host"],
                port=port,
                use_tls=(port == 465),
                start_tls=(port != 465),
                timeout=30,
            )
            await smtp.connect()
            await smtp.login(
                account["effective_smtp_username"],
                account["effective_smtp_password"],
            )
            response = await smtp.send_message(msg)
            await smtp.quit()
            return msg["Message-ID"] or response[1]

    except SMTPAuthenticationError as e:
        raise SmtpSendError("auth_error", str(e))
    except SMTPRecipientsRefused as e:
        raise SmtpSendError("recipient_rejected", str(e))
    except (SMTPConnectError, asyncio.TimeoutError, OSError) as e:
        raise SmtpSendError("connection_error", str(e))
