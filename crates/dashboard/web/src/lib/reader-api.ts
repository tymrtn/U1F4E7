// Reader-pane API helpers — typed wrappers around the raw request() core.
//
// Owners note: these belong here (not in api.ts) because the reader agent owns
// this file this sprint. When the file fence lifts, the flag/thread helpers are
// good merge candidates for api.ts.
//
// Endpoints verified against crates/dashboard/src/handlers/:
//   GET  /api/accounts/{id}/messages/{uid}?folder=   → MessageDetailResponse   (messages.rs read)
//   POST /api/accounts/{id}/messages/{uid}/flags      → FlagsResponse           (messages.rs flags)
//   GET  /api/accounts/{id}/threads/{message_id}      → ThreadResponse          (threads.rs show_by_message_id)
//   GET  /api/accounts/{id}/messages/{uid}/attachments/{filename}?folder= → binary (attachments.rs download)

import { request, type MessageDetail, type RequestOptions } from './api';

// ── Additional attachment type ────────────────────────────────────────

/**
 * Attachment metadata as embedded in a MessageDetail.
 * Mirrors crates/store/src/models.rs AttachmentMeta.
 */
export interface AttachmentMeta {
  filename: string;
  content_type: string;
  size: number;
  content_id?: string | null;
}

/** MessageDetail with the attachments field properly typed. */
export interface MessageDetailFull extends Omit<MessageDetail, 'attachments'> {
  attachments?: AttachmentMeta[] | null;
}

/** Wrapper matching GET /api/accounts/{id}/messages/{uid} response shape. */
export interface MessageDetailFullResponse {
  message: MessageDetailFull;
}

// ── Thread types ──────────────────────────────────────────────────────

/**
 * A single message in a thread listing from GET /api/accounts/{id}/threads/{message_id}.
 *
 * Mirrors `ThreadMessage` in crates/store/src/models.rs field-for-field — the
 * handler serializes that struct directly. It is NOT the shape of a mailbox
 * listing row: there are no `flags` and no `size` here, and the address fields
 * are `from_address`/`to_addresses`. Reading `from_addr`/`flags` off one of
 * these yields `undefined` and throws at the first `.match`/`.some`.
 */
export interface ThreadMessage {
  id: number;
  thread_id: string;
  uid: number;
  message_id: string | null;
  in_reply_to: string | null;
  references: string | null;
  folder: string;
  from_address: string | null;
  to_addresses: string | null;
  date: string | null;
  subject: string | null;
  is_outbound: boolean;
  snippet: string | null;
}

export interface ThreadResponse {
  thread_id: string;
  messages: ThreadMessage[];
}

// ── Flags response ─────────────────────────────────────────────────────

export interface FlagsResponse {
  ok: boolean;
  uid: number;
  added: string[];
  removed: string[];
}

// ── Typed request helpers ─────────────────────────────────────────────

/**
 * Fetch a full message detail. Uses BODY.PEEK[] so the read never marks the
 * message as \Seen — that is an explicit invariant.
 */
export function fetchMessageDetail(
  accountId: string,
  uid: number,
  folder = 'INBOX',
  o?: RequestOptions
): Promise<MessageDetailFullResponse> {
  return request<MessageDetailFullResponse>(
    `/accounts/${encodeURIComponent(accountId)}/messages/${uid}`,
    { ...o, query: { folder } }
  );
}

/**
 * POST flags to /api/accounts/{id}/messages/{uid}/flags.
 * Adds and removes IMAP flags by name (e.g. '\\Seen').
 */
export function postFlags(
  accountId: string,
  uid: number,
  folder: string,
  add: string[],
  remove: string[],
  o?: RequestOptions
): Promise<FlagsResponse> {
  return request<FlagsResponse>(
    `/accounts/${encodeURIComponent(accountId)}/messages/${uid}/flags`,
    { ...o, method: 'POST', body: { folder, add, remove } }
  );
}

/**
 * Fetch the thread for a message by its Message-ID.
 * GET /api/accounts/{id}/threads/{message_id}
 * Returns null if the message has no thread (404 is treated as no-thread, not error).
 */
export async function fetchThread(
  accountId: string,
  messageId: string,
  o?: RequestOptions
): Promise<ThreadResponse | null> {
  try {
    return await request<ThreadResponse>(
      `/accounts/${encodeURIComponent(accountId)}/threads/${encodeURIComponent(messageId)}`,
      o
    );
  } catch (err) {
    const e = err as { status?: number };
    if (e?.status === 404) return null;
    throw err;
  }
}

/**
 * Build the URL to download an attachment directly from the browser.
 * GET /api/accounts/{id}/messages/{uid}/attachments/{filename}?folder=
 */
export function attachmentDownloadUrl(
  accountId: string,
  uid: number,
  filename: string,
  folder = 'INBOX'
): string {
  const base = `/api/accounts/${encodeURIComponent(accountId)}/messages/${uid}/attachments/${encodeURIComponent(filename)}`;
  return `${base}?folder=${encodeURIComponent(folder)}`;
}

// ── Utilities ─────────────────────────────────────────────────────────

/** Format a byte count as a human-readable string (B / KB / MB). */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * Reduce a Message-ID to its bare `local@domain` form.
 *
 * Drafts persist the RFC 5322 bracketed form while the thread index stores
 * bracket-free ids, so anything comparing the two — or putting one in a
 * `/threads/{message_id}` path — has to normalize first.
 * Mirrors `normalize_message_id` in crates/store/src/threads.rs.
 */
export function normalizeMessageId(messageId: string): string {
  return messageId.trim().replace(/^<+/, '').replace(/>+$/, '').trim();
}

/** Return true when the flags array contains \\Seen. Case-insensitive. */
export function isSeen(flags: string[]): boolean {
  return flags.some((f) => f.toLowerCase() === '\\seen');
}
