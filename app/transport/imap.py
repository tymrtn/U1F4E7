# Copyright (c) 2026 Tyler Martin
# Licensed under FSL-1.1-ALv2 (see LICENSE)

import asyncio
import email
import imaplib
import logging
from dataclasses import dataclass, field
from email.header import decode_header
from typing import Optional

logger = logging.getLogger(__name__)


@dataclass
class InboundMessage:
    uid: str
    message_id: Optional[str]
    from_addr: str
    to_addr: str
    subject: str
    text_body: str
    html_body: Optional[str]
    in_reply_to: Optional[str]
    references: Optional[str]
    date: Optional[str]


def _decode_header_value(value: str) -> str:
    if not value:
        return ""
    parts = decode_header(value)
    decoded = []
    for part, charset in parts:
        if isinstance(part, bytes):
            decoded.append(part.decode(charset or "utf-8", errors="replace"))
        else:
            decoded.append(part)
    return " ".join(decoded)


def _extract_body(msg: email.message.Message) -> tuple[str, Optional[str]]:
    text_body = ""
    html_body = None

    if msg.is_multipart():
        for part in msg.walk():
            content_type = part.get_content_type()
            disposition = str(part.get("Content-Disposition", ""))
            if "attachment" in disposition:
                continue
            if content_type == "text/plain":
                payload = part.get_payload(decode=True)
                if payload:
                    charset = part.get_content_charset() or "utf-8"
                    text_body = payload.decode(charset, errors="replace")
            elif content_type == "text/html":
                payload = part.get_payload(decode=True)
                if payload:
                    charset = part.get_content_charset() or "utf-8"
                    html_body = payload.decode(charset, errors="replace")
    else:
        payload = msg.get_payload(decode=True)
        if payload:
            charset = msg.get_content_charset() or "utf-8"
            content = payload.decode(charset, errors="replace")
            if msg.get_content_type() == "text/html":
                html_body = content
            else:
                text_body = content

    return text_body, html_body


def _fetch_unread_sync(
    host: str,
    port: int,
    username: str,
    password: str,
    folder: str = "INBOX",
) -> list[InboundMessage]:
    messages = []
    conn = imaplib.IMAP4_SSL(host, port)
    try:
        conn.login(username, password)
        conn.select(folder)
        _, data = conn.uid("SEARCH", None, "UNSEEN")
        uids = data[0].split() if data[0] else []

        for uid_bytes in uids:
            uid = uid_bytes.decode()
            _, msg_data = conn.uid("FETCH", uid, "(RFC822)")
            if not msg_data or not msg_data[0]:
                continue
            raw = msg_data[0][1]
            msg = email.message_from_bytes(raw)
            text_body, html_body = _extract_body(msg)

            messages.append(InboundMessage(
                uid=uid,
                message_id=msg.get("Message-ID"),
                from_addr=_decode_header_value(msg.get("From", "")),
                to_addr=_decode_header_value(msg.get("To", "")),
                subject=_decode_header_value(msg.get("Subject", "")),
                text_body=text_body,
                html_body=html_body,
                in_reply_to=msg.get("In-Reply-To"),
                references=msg.get("References"),
                date=msg.get("Date"),
            ))

        return messages
    finally:
        try:
            conn.close()
        except Exception:
            pass
        conn.logout()


def _mark_seen_sync(
    host: str,
    port: int,
    username: str,
    password: str,
    uid: str,
    folder: str = "INBOX",
) -> None:
    conn = imaplib.IMAP4_SSL(host, port)
    try:
        conn.login(username, password)
        conn.select(folder)
        conn.uid("STORE", uid, "+FLAGS", "\\Seen")
    finally:
        try:
            conn.close()
        except Exception:
            pass
        conn.logout()


async def fetch_unread(account: dict, folder: str = "INBOX") -> list[InboundMessage]:
    return await asyncio.to_thread(
        _fetch_unread_sync,
        host=account["imap_host"],
        port=account["imap_port"],
        username=account["effective_imap_username"],
        password=account["effective_imap_password"],
        folder=folder,
    )


async def mark_seen(account: dict, uid: str, folder: str = "INBOX") -> None:
    await asyncio.to_thread(
        _mark_seen_sync,
        host=account["imap_host"],
        port=account["imap_port"],
        username=account["effective_imap_username"],
        password=account["effective_imap_password"],
        uid=uid,
        folder=folder,
    )


# --- Thread search ---


def _search_thread_sync(
    host: str,
    port: int,
    username: str,
    password: str,
    message_id: str,
    folder: str = "INBOX",
) -> list[dict]:
    """Search for all messages in a thread by following References/In-Reply-To headers."""
    conn = imaplib.IMAP4_SSL(host, port)
    try:
        conn.login(username, password)
        conn.select(folder, readonly=True)

        seen_uids = set()
        to_search = [message_id]
        thread_messages = []

        while to_search:
            target_id = to_search.pop(0)
            # Search for messages referencing this ID
            for header in ("References", "In-Reply-To"):
                try:
                    _, data = conn.uid(
                        "SEARCH", None,
                        f'HEADER "{header}" "{target_id}"',
                    )
                    uids = data[0].split() if data[0] else []
                    for uid_bytes in uids:
                        uid = uid_bytes.decode()
                        if uid not in seen_uids:
                            seen_uids.add(uid)
                except imaplib.IMAP4.error:
                    continue

            # Also search for the message itself by Message-ID
            try:
                _, data = conn.uid(
                    "SEARCH", None,
                    f'HEADER "Message-ID" "{target_id}"',
                )
                uids = data[0].split() if data[0] else []
                for uid_bytes in uids:
                    uid = uid_bytes.decode()
                    if uid not in seen_uids:
                        seen_uids.add(uid)
            except imaplib.IMAP4.error:
                pass

        # Fetch full messages for all found UIDs
        for uid in sorted(seen_uids):
            try:
                _, msg_data = conn.uid("FETCH", uid, "(RFC822)")
                if not msg_data or not msg_data[0]:
                    continue
                raw = msg_data[0][1]
                msg = email.message_from_bytes(raw)
                text_body, html_body = _extract_body(msg)

                # Queue referenced messages for search
                refs = msg.get("References", "")
                in_reply = msg.get("In-Reply-To", "")
                for ref_id in _parse_message_ids(refs) + _parse_message_ids(in_reply):
                    if ref_id not in [m.get("message_id") for m in thread_messages]:
                        # Don't re-search IDs we've already found
                        pass

                thread_messages.append({
                    "uid": uid,
                    "message_id": msg.get("Message-ID"),
                    "from_addr": _decode_header_value(msg.get("From", "")),
                    "to_addr": _decode_header_value(msg.get("To", "")),
                    "subject": _decode_header_value(msg.get("Subject", "")),
                    "date": msg.get("Date"),
                    "in_reply_to": msg.get("In-Reply-To"),
                    "references": msg.get("References"),
                    "text_body": text_body,
                    "html_body": html_body,
                })
            except imaplib.IMAP4.error:
                continue

        # Sort by date (oldest first)
        thread_messages.sort(key=lambda m: m.get("date") or "")
        return thread_messages
    finally:
        try:
            conn.close()
        except Exception:
            pass
        conn.logout()


def _parse_message_ids(header_value: str) -> list[str]:
    """Extract message IDs from References or In-Reply-To headers."""
    if not header_value:
        return []
    import re
    return re.findall(r"<[^>]+>", header_value)


async def get_thread(
    account: dict,
    message_id: str,
    folder: str = "INBOX",
) -> list[dict]:
    args = _imap_account_args(account)
    try:
        return await asyncio.to_thread(
            _search_thread_sync, **args, message_id=message_id, folder=folder,
        )
    except imaplib.IMAP4.error as e:
        raise ImapError("imap_error", str(e))
    except OSError as e:
        raise ImapError("connection_error", str(e))


# --- Inbox API extensions ---


@dataclass
class MessageSummary:
    uid: str
    message_id: Optional[str]
    from_addr: str
    to_addr: str
    subject: str
    date: Optional[str]
    flags: list[str] = field(default_factory=list)
    size: int = 0


@dataclass
class AttachmentInfo:
    filename: str
    content_type: str
    size: int


def _extract_attachments(msg: email.message.Message) -> list[dict]:
    attachments = []
    if not msg.is_multipart():
        return attachments
    for part in msg.walk():
        disposition = str(part.get("Content-Disposition", ""))
        if "attachment" not in disposition:
            continue
        filename = part.get_filename() or "untitled"
        filename = _decode_header_value(filename)
        payload = part.get_payload(decode=True)
        size = len(payload) if payload else 0
        attachments.append({
            "filename": filename,
            "content_type": part.get_content_type(),
            "size": size,
        })
    return attachments


def _search_sync(
    host: str,
    port: int,
    username: str,
    password: str,
    folder: str = "INBOX",
    query: str = "ALL",
) -> list[str]:
    conn = imaplib.IMAP4_SSL(host, port)
    try:
        conn.login(username, password)
        conn.select(folder, readonly=True)
        _, data = conn.uid("SEARCH", None, query)
        uids = data[0].split() if data[0] else []
        return [u.decode() for u in uids]
    finally:
        try:
            conn.close()
        except Exception:
            pass
        conn.logout()


def _fetch_summaries_sync(
    host: str,
    port: int,
    username: str,
    password: str,
    folder: str,
    uids: list[str],
) -> list[dict]:
    if not uids:
        return []
    conn = imaplib.IMAP4_SSL(host, port)
    try:
        conn.login(username, password)
        conn.select(folder, readonly=True)
        uid_str = ",".join(uids)
        _, data = conn.uid("FETCH", uid_str, "(FLAGS RFC822.SIZE BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE MESSAGE-ID)])")
        results = []
        i = 0
        while i < len(data):
            item = data[i]
            if isinstance(item, tuple) and len(item) == 2:
                meta_line = item[0].decode() if isinstance(item[0], bytes) else item[0]
                header_bytes = item[1]

                # Extract UID from meta line
                uid = ""
                if "UID" in meta_line.upper():
                    parts = meta_line.upper().split("UID")
                    if len(parts) > 1:
                        uid = parts[1].strip().split()[0].strip(")")
                # Extract FLAGS
                flags = []
                if "FLAGS" in meta_line:
                    flags_start = meta_line.index("FLAGS") + 5
                    flags_section = meta_line[flags_start:]
                    if "(" in flags_section:
                        inner = flags_section[flags_section.index("(") + 1:]
                        if ")" in inner:
                            inner = inner[:inner.index(")")]
                        flags = [f.strip() for f in inner.split() if f.strip()]
                # Extract SIZE
                size = 0
                if "RFC822.SIZE" in meta_line.upper():
                    parts = meta_line.upper().split("RFC822.SIZE")
                    if len(parts) > 1:
                        size_str = parts[1].strip().split()[0].strip(")")
                        try:
                            size = int(size_str)
                        except ValueError:
                            pass

                msg = email.message_from_bytes(header_bytes)
                results.append({
                    "uid": uid,
                    "message_id": msg.get("Message-ID"),
                    "from_addr": _decode_header_value(msg.get("From", "")),
                    "to_addr": _decode_header_value(msg.get("To", "")),
                    "subject": _decode_header_value(msg.get("Subject", "")),
                    "date": msg.get("Date"),
                    "flags": flags,
                    "size": size,
                })
            i += 1
        return results
    finally:
        try:
            conn.close()
        except Exception:
            pass
        conn.logout()


def _fetch_message_sync(
    host: str,
    port: int,
    username: str,
    password: str,
    folder: str,
    uid: str,
) -> Optional[dict]:
    conn = imaplib.IMAP4_SSL(host, port)
    try:
        conn.login(username, password)
        conn.select(folder, readonly=True)
        _, msg_data = conn.uid("FETCH", uid, "(RFC822)")
        if not msg_data or not msg_data[0]:
            return None
        raw = msg_data[0][1]
        msg = email.message_from_bytes(raw)
        text_body, html_body = _extract_body(msg)
        attachments = _extract_attachments(msg)

        return {
            "uid": uid,
            "message_id": msg.get("Message-ID"),
            "from_addr": _decode_header_value(msg.get("From", "")),
            "to_addr": _decode_header_value(msg.get("To", "")),
            "subject": _decode_header_value(msg.get("Subject", "")),
            "date": msg.get("Date"),
            "in_reply_to": msg.get("In-Reply-To"),
            "references": msg.get("References"),
            "text_body": text_body,
            "html_body": html_body,
            "attachments": attachments,
        }
    finally:
        try:
            conn.close()
        except Exception:
            pass
        conn.logout()


def _list_folders_sync(
    host: str,
    port: int,
    username: str,
    password: str,
) -> list[str]:
    conn = imaplib.IMAP4_SSL(host, port)
    try:
        conn.login(username, password)
        _, data = conn.list()
        folders = []
        for item in data:
            if isinstance(item, bytes):
                # Format: (\\flags) "delimiter" "name"
                decoded = item.decode("utf-8", errors="replace")
                # Extract folder name (last quoted string or last token)
                parts = decoded.rsplit('"', 2)
                if len(parts) >= 2:
                    folders.append(parts[-2])
                else:
                    folders.append(decoded.rsplit(" ", 1)[-1])
        return folders
    finally:
        conn.logout()


# --- Async wrappers for inbox API ---


class ImapError(Exception):
    def __init__(self, error_type: str, message: str):
        self.error_type = error_type
        self.message = message
        super().__init__(message)


def _imap_account_args(account: dict) -> dict:
    return {
        "host": account["imap_host"],
        "port": account["imap_port"],
        "username": account["effective_imap_username"],
        "password": account["effective_imap_password"],
    }


async def search_messages(
    account: dict,
    folder: str = "INBOX",
    query: str = "ALL",
    limit: int = 50,
    offset: int = 0,
) -> list[dict]:
    args = _imap_account_args(account)
    try:
        uids = await asyncio.to_thread(
            _search_sync, **args, folder=folder, query=query,
        )
    except imaplib.IMAP4.error as e:
        raise ImapError("imap_error", str(e))
    except OSError as e:
        raise ImapError("connection_error", str(e))

    # Reverse for newest-first, then paginate
    uids.reverse()
    page = uids[offset:offset + limit]
    if not page:
        return []

    try:
        return await asyncio.to_thread(
            _fetch_summaries_sync, **args, folder=folder, uids=page,
        )
    except imaplib.IMAP4.error as e:
        raise ImapError("imap_error", str(e))
    except OSError as e:
        raise ImapError("connection_error", str(e))


async def fetch_message(
    account: dict,
    folder: str,
    uid: str,
) -> Optional[dict]:
    args = _imap_account_args(account)
    try:
        return await asyncio.to_thread(
            _fetch_message_sync, **args, folder=folder, uid=uid,
        )
    except imaplib.IMAP4.error as e:
        raise ImapError("imap_error", str(e))
    except OSError as e:
        raise ImapError("connection_error", str(e))


async def list_folders(account: dict) -> list[str]:
    args = _imap_account_args(account)
    try:
        return await asyncio.to_thread(_list_folders_sync, **args)
    except imaplib.IMAP4.error as e:
        raise ImapError("imap_error", str(e))
    except OSError as e:
        raise ImapError("connection_error", str(e))
