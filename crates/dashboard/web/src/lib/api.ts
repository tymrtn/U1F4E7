// Typed API client for the Envelope dashboard REST surface.
//
// Types are hand-written from crates/dashboard/src/handlers/* and
// docs/schemas/envelope.agent_contract.v1.json. They cover the surfaces the
// v2 webmail needs first (accounts, unified inbox, folder messages, message
// detail, drafts, cockpit). They are intentionally partial — extend as new
// panes come online, don't try to mirror the whole backend up front.
//
// The request() core primes a CSRF token lazily from GET /api/csrf, attaches
// X-Envelope-CSRF on mutating methods, and retries once on a
// `dashboard_csrf_required` 403 (the backend can rotate the token). Errors
// surface as EnvelopeApiError carrying the stable `code` field.

const API_BASE = '/api';

/** Mutating HTTP methods that require a CSRF token when not bearer-authed. */
const MUTATING = new Set(['POST', 'PUT', 'DELETE', 'PATCH']);

/** Stable error code returned by the backend when a CSRF token is required. */
export const CSRF_REQUIRED_CODE = 'dashboard_csrf_required';

/** A structured API error carrying the backend's stable `code` string. */
export class EnvelopeApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly body: unknown;

  constructor(status: number, code: string, message: string, body: unknown) {
    super(message);
    this.name = 'EnvelopeApiError';
    this.status = status;
    this.code = code;
    this.body = body;
  }
}

// ── CSRF token cache ──────────────────────────────────────────────────

let csrfToken: string | null = null;
let csrfInFlight: Promise<string> | null = null;

/** Fetch (and cache) a CSRF token. Coalesces concurrent callers. */
async function primeCsrf(fetchImpl: typeof fetch): Promise<string> {
  if (csrfToken) return csrfToken;
  if (csrfInFlight) return csrfInFlight;

  csrfInFlight = (async () => {
    const res = await fetchImpl(`${API_BASE}/csrf`, {
      method: 'GET',
      credentials: 'same-origin'
    });
    if (!res.ok) {
      throw new EnvelopeApiError(
        res.status,
        'dashboard_csrf_mint_failed',
        `failed to mint CSRF token (${res.status})`,
        await safeJson(res)
      );
    }
    const data = (await res.json()) as { token?: string };
    if (!data.token) {
      throw new EnvelopeApiError(
        res.status,
        'dashboard_csrf_mint_failed',
        'CSRF endpoint returned no token',
        data
      );
    }
    csrfToken = data.token;
    return csrfToken;
  })();

  try {
    return await csrfInFlight;
  } finally {
    csrfInFlight = null;
  }
}

/** Drop the cached CSRF token so the next mutating call re-primes. */
export function resetCsrf(): void {
  csrfToken = null;
}

async function safeJson(res: Response): Promise<unknown> {
  try {
    return await res.clone().json();
  } catch {
    return undefined;
  }
}

// ── Core request ──────────────────────────────────────────────────────

export interface RequestOptions {
  method?: string;
  /** JSON body — serialized and sent as application/json. */
  body?: unknown;
  query?: Record<string, string | number | boolean | undefined>;
  signal?: AbortSignal;
  /** Injectable fetch for tests. Defaults to the global fetch. */
  fetchImpl?: typeof fetch;
}

/**
 * Perform a request against the dashboard `/api` surface.
 *
 * - Primes the CSRF token lazily before the first mutating call.
 * - Attaches `X-Envelope-CSRF` on POST/PUT/DELETE/PATCH.
 * - Retries exactly once on a 403 with code `dashboard_csrf_required`, after
 *   re-priming the token (handles backend token rotation).
 */
export async function request<T>(path: string, opts: RequestOptions = {}): Promise<T> {
  const fetchImpl = opts.fetchImpl ?? fetch;
  const method = (opts.method ?? 'GET').toUpperCase();
  const url = buildUrl(path, opts.query);

  const attempt = async (retrying: boolean): Promise<T> => {
    const headers: Record<string, string> = { Accept: 'application/json' };

    if (opts.body !== undefined) {
      headers['Content-Type'] = 'application/json';
    }

    if (MUTATING.has(method)) {
      headers['X-Envelope-CSRF'] = await primeCsrf(fetchImpl);
    }

    const res = await fetchImpl(url, {
      method,
      headers,
      credentials: 'same-origin',
      signal: opts.signal,
      body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined
    });

    if (res.status === 403 && !retrying) {
      const errBody = (await safeJson(res)) as { code?: string } | undefined;
      if (errBody?.code === CSRF_REQUIRED_CODE) {
        resetCsrf();
        return attempt(true);
      }
    }

    if (!res.ok) {
      const errBody = (await safeJson(res)) as
        | { code?: string; error?: string; message?: string }
        | undefined;
      const code = errBody?.code ?? `http_${res.status}`;
      const message = errBody?.message ?? errBody?.error ?? `request failed (${res.status})`;
      throw new EnvelopeApiError(res.status, code, message, errBody);
    }

    if (res.status === 204) return undefined as T;
    return (await res.json()) as T;
  };

  return attempt(false);
}

function buildUrl(path: string, query?: RequestOptions['query']): string {
  const base = path.startsWith('/api') ? path : `${API_BASE}${path}`;
  if (!query) return base;
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) {
    if (v !== undefined) params.set(k, String(v));
  }
  const qs = params.toString();
  return qs ? `${base}?${qs}` : base;
}

// ── Domain types (partial, extend as needed) ──────────────────────────

export interface Account {
  id: string;
  name: string;
  username: string;
  domain: string;
  smtp_host: string;
  smtp_port: number;
  imap_host: string;
  imap_port: number;
  /** Optional friendly name; falls back to `name` in the UI. */
  display_name?: string | null;
  smtp_username?: string | null;
  imap_username?: string | null;
  created_at?: string;
}

/** Core message summary shared across list/unified/folder surfaces. */
export interface MessageSummary {
  uid: number;
  message_id: string | null;
  from_addr: string;
  to_addr: string;
  subject: string;
  date: string | null;
  flags: string[];
  size: number;
}

export interface UnifiedInboxMessage extends MessageSummary {
  unread: boolean;
  account_id: string;
  account_username: string;
  account_display_name: string | null;
  folder: string;
  uidvalidity: number;
  snippet: string | null;
  thread_id: string | null;
  indexed_at: string | null;
  index_freshness: string;
}

export interface UnifiedInboxError {
  account_id: string;
  account_username: string;
  account_display_name: string | null;
  folder: string;
  error: string;
}

export interface UnifiedInboxResponse {
  scope: 'unified_inbox';
  status: string;
  folder: string;
  limit: number;
  messages: UnifiedInboxMessage[];
  accounts: unknown[];
  unread_count: number;
  freshness: string;
  errors?: UnifiedInboxError[];
}

export interface FolderMessagesResponse {
  messages: MessageSummary[];
}

/**
 * Message detail. Field names mirror the `store::Message` struct that the
 * `/accounts/{id}/messages/{uid}` handler flattens into `{ "message": … }`:
 * body fields are `text_body` / `html_body` (NOT `*_content`). The handler
 * fetches with `BODY.PEEK[]`, so loading a message never marks it read.
 */
export interface MessageDetail {
  uid: number;
  message_id: string | null;
  from_addr: string;
  to_addr: string;
  cc_addr?: string | null;
  to_addrs?: string[];
  cc_addrs?: string[];
  subject: string;
  date: string | null;
  flags: string[];
  text_body?: string | null;
  html_body?: string | null;
  in_reply_to?: string | null;
  references?: string | null;
  attachments?: unknown[];
  /** Dashboard-added: derived from the absence of the \Seen flag. */
  unread?: boolean;
}

export interface MessageDetailResponse {
  message: MessageDetail;
}

/**
 * GET /api/health — identity of the *running* dashboard process.
 *
 * `version` is the live binary's version (handlers/health.rs reads
 * `BuildInfo::current()`), which is exactly why the UI must render it rather
 * than a compiled-in string: a stale launchd service has to visibly report its
 * own version. The path/backend fields are only returned to authorized callers,
 * so they are optional here.
 */
export interface HealthResponse {
  status: string;
  service: string;
  version: string;
  binary_path?: string;
  credential_backend?: string;
  database_path?: string;
  app_data_dir?: string;
}

/** GET /api/stats aggregate counts (folder-agnostic, whole-install). */
export interface StatsResponse {
  accounts: number;
  snoozed: number;
  drafts: number;
}

/** POST /api/accounts/{id}/verify — IMAP reconnect probe result. */
export interface VerifyResult {
  ok: boolean;
  imap: boolean;
  smtp: boolean;
  error: string | null;
}

/** A single failed-auth record from the cockpit `auth.items` stream. */
export interface FailedAuthItem {
  id: string;
  account_id: string;
  backend: string;
  reason: string;
  retry_guidance: string | null;
  created_at: string;
}

/** A failed action from the cockpit `actions.failed` stream (loose shape). */
export interface FailedActionItem {
  account_id?: string;
  action_status?: string;
  [key: string]: unknown;
}

/**
 * Mirrors `store::DraftStatus` (serde snake_case). `sending`/`syncing` are
 * durable claims held by the send sweep and the IMAP sync, and
 * `delivery_uncertain` is the terminal-recovery park — none of the three are
 * editable, so surfaces must render them read-only rather than assume a draft
 * is always open for editing.
 */
export type DraftStatus =
  | 'draft'
  | 'pending_review'
  | 'sending'
  | 'syncing'
  | 'blocked'
  | 'delivery_uncertain'
  | 'sent'
  | 'discarded';

/** Statuses the store's content-edit guard accepts (`update_draft_content_inner`). */
export const EDITABLE_DRAFT_STATUSES: readonly DraftStatus[] = [
  'draft',
  'pending_review',
  'blocked'
];

/**
 * Statuses `queue_draft_with_human_approval` will promote into the outbox.
 * `blocked` is deliberately excluded: it means changes were requested, and it
 * has to be approved back into `draft` before it can be queued.
 */
export const SENDABLE_DRAFT_STATUSES: readonly DraftStatus[] = ['draft', 'pending_review'];

export function isEditableDraftStatus(status: DraftStatus): boolean {
  return EDITABLE_DRAFT_STATUSES.includes(status);
}

export function isSendableDraftStatus(status: DraftStatus): boolean {
  return SENDABLE_DRAFT_STATUSES.includes(status);
}

export interface Draft {
  id: string;
  account_id: string;
  status: DraftStatus;
  to_addr: string;
  cc_addr: string | null;
  bcc_addr: string | null;
  reply_to: string | null;
  subject: string | null;
  text_content: string | null;
  html_content: string | null;
  in_reply_to: string | null;
  metadata: Record<string, unknown> | null;
  attachments: unknown[];
  message_id: string | null;
  send_after: string | null;
  snoozed_until: string | null;
  created_at: string;
  updated_at: string;
  sent_at: string | null;
  created_by: string | null;
  imap_uid?: number;
  /**
   * Monotonic revision counter bumped by every content-relevant mutation.
   * Edit/approve/send must echo the revision they were shown back as
   * `expected_revision`; the server returns 409 instead of overwriting content
   * the operator never saw.
   */
  revision: number;
}

export interface DraftsResponse {
  drafts: Draft[];
}

export interface DraftResponse {
  draft: Draft;
  account?: Account;
  dashboard_path?: string;
  dashboard_url?: string;
  review_url?: string | null;
}

/**
 * Body for POST /api/accounts/{id}/drafts/{draftId}/edit. Mirrors
 * `DraftEditRequest`, which is `deny_unknown_fields` — send these keys only.
 *
 * `text_content` and `html_content` are one unit: supplying either replaces the
 * body pair and CLEARS the omitted alternate, so a single-format editor cannot
 * leave a stale alternate behind for `multipart/alternative` delivery to
 * surface instead of the edit. Supplying neither leaves both bodies untouched.
 */
export interface DraftEditBody {
  expected_revision: number;
  to_addr?: string;
  cc_addr?: string;
  bcc_addr?: string;
  subject?: string;
  text_content?: string;
  html_content?: string;
}

export interface DraftEditResponse {
  draft: Draft;
  status: string;
}

/**
 * Body for POST /api/accounts/{id}/drafts/{draftId}/send. Mirrors
 * `DraftSendRequest` (`deny_unknown_fields`).
 *
 * `confirm` must be an explicit human decision — the backend rejects the call
 * with 400 otherwise. There is no immediate-SMTP path here: the endpoint queues
 * the draft into the outbox cooldown and the shared scheduled-send sweep
 * performs the real send behind the Governor gate.
 */
export interface DraftSendBody {
  confirm: boolean;
  expected_revision: number;
  cooldown_seconds?: number;
}

export interface DraftQueuedResponse {
  draft_id: string;
  sent: boolean;
  status: string;
  send_after: string;
  cooldown_seconds: number;
  queued_reason_code: string;
  queued_reason: string;
}

/**
 * Agent Cockpit aggregate. The full payload is broad; we type only the slices
 * the v2 rail derives account health from. `auth.items` and `actions.failed`
 * each carry an `account_id`, so the global (no-account) call is enough to
 * badge every account. Shape verified against handlers/cockpit.rs.
 */
export interface CockpitResponse {
  account_status?: string;
  auth?: { status?: string; items?: FailedAuthItem[] };
  actions?: { recent?: unknown[]; failed?: FailedActionItem[] };
  summary?: { failed_actions?: number; [key: string]: unknown };
  generated_at?: string;
  [key: string]: unknown;
}

// ── Compose / reply request + response types ──────────────────────────

/** Body for POST /api/accounts/{id}/compose. Mirrors ComposeRequest in compose.rs. */
export interface ComposeBody {
  to: string;
  subject: string;
  text?: string | null;
  html?: string | null;
  cc?: string | null;
  bcc?: string | null;
  reply_to?: string | null;
  attachments?: ComposeAttachment[];
}

export interface ComposeAttachment {
  filename: string;
  content_type: string;
  data_b64: string;
}

/** Body for POST /api/accounts/{id}/compose/reply. Mirrors ReplyRequest in compose.rs. */
export interface ReplyBody {
  parent_uid: number;
  parent_folder?: string;
  reply_all?: boolean;
  text?: string | null;
  html?: string | null;
  attachments?: ComposeAttachment[];
}

/**
 * Response from compose / compose/reply endpoints. The backend queues with an
 * outbox cooldown; `cooldown_seconds` tells the UI how long the undo window is.
 * When `cooldown_seconds` is 0 the draft may have been swept immediately — show
 * no undo toast in that case.
 */
export interface ComposeResponse {
  ok: boolean;
  status: 'queued' | string;
  draft_id: string;
  send_after: string;
  cooldown_seconds: number;
  in_reply_to?: string | null;
  references?: string[] | null;
}

// ── Typed endpoint helpers ────────────────────────────────────────────

export const api = {
  listAccounts(o?: RequestOptions): Promise<{ accounts: Account[] }> {
    return request('/accounts', o);
  },

  unifiedInbox(limit = 50, o?: RequestOptions): Promise<UnifiedInboxResponse> {
    return request('/messages/unified', { ...o, query: { limit } });
  },

  refreshUnifiedInbox(limit = 50, o?: RequestOptions): Promise<UnifiedInboxResponse> {
    return request('/messages/unified/refresh', { ...o, method: 'POST', query: { limit } });
  },

  folderMessages(
    accountId: string,
    folder: string,
    o?: RequestOptions
  ): Promise<FolderMessagesResponse> {
    return request(`/accounts/${encodeURIComponent(accountId)}/messages`, {
      ...o,
      query: { folder }
    });
  },

  message(
    accountId: string,
    uid: number,
    folder = 'INBOX',
    o?: RequestOptions
  ): Promise<MessageDetailResponse> {
    return request(`/accounts/${encodeURIComponent(accountId)}/messages/${uid}`, {
      ...o,
      query: { folder }
    });
  },

  drafts(accountId: string, o?: RequestOptions): Promise<DraftsResponse> {
    return request(`/accounts/${encodeURIComponent(accountId)}/drafts`, o);
  },

  draft(accountId: string, draftId: string, o?: RequestOptions): Promise<DraftResponse> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/drafts/${encodeURIComponent(draftId)}`,
      o
    );
  },

  /**
   * Agent Cockpit aggregate. Passing no `accountId` hits the global
   * `/api/cockpit` (all accounts) — used by the rail to badge every account
   * from one read. Passing an id scopes the payload to that account.
   */
  cockpit(accountId?: string, o?: RequestOptions): Promise<CockpitResponse> {
    return request('/cockpit', {
      ...o,
      query: accountId ? { account_id: accountId } : undefined
    });
  },

  stats(o?: RequestOptions): Promise<StatsResponse> {
    return request('/stats', o);
  },

  /** GET /api/health — running-service identity (version, and paths if authorized). */
  health(o?: RequestOptions): Promise<HealthResponse> {
    return request('/health', o);
  },

  /** POST /api/accounts/{id}/verify — reconnect probe (IMAP auth check). */
  verifyAccount(accountId: string, o?: RequestOptions): Promise<VerifyResult> {
    return request(`/accounts/${encodeURIComponent(accountId)}/verify`, {
      ...o,
      method: 'POST'
    });
  },

  /** DELETE /api/accounts/{id} — remove an account and its stored credential. */
  deleteAccount(accountId: string, o?: RequestOptions): Promise<{ deleted: string }> {
    return request(`/accounts/${encodeURIComponent(accountId)}`, {
      ...o,
      method: 'DELETE'
    });
  },

  /** GET /api/accounts/{id}/folders — IMAP folder list with STATUS stats. */
  folders(accountId: string, o?: RequestOptions): Promise<FoldersResponse> {
    return request(`/accounts/${encodeURIComponent(accountId)}/folders`, o);
  },

  /** GET /api/accounts/{id}/snoozed — snoozed messages for an account. */
  snoozed(accountId: string, o?: RequestOptions): Promise<SnoozedResponse> {
    return request(`/accounts/${encodeURIComponent(accountId)}/snoozed`, o);
  },

  /**
   * POST /api/accounts/{id}/messages/{uid}/flags
   * Adds and/or removes IMAP flags on a single message.
   */
  messageFlags(
    accountId: string,
    uid: number,
    opts: { folder?: string; add?: string[]; remove?: string[] },
    o?: RequestOptions
  ): Promise<{ ok: boolean; uid: number; added: string[]; removed: string[] }> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/messages/${uid}/flags`,
      {
        ...o,
        method: 'POST',
        body: { folder: opts.folder ?? 'INBOX', add: opts.add ?? [], remove: opts.remove ?? [] }
      }
    );
  },

  /**
   * POST /api/accounts/{id}/messages/{uid}/move
   * Moves a message to a target folder.
   */
  messageMove(
    accountId: string,
    uid: number,
    opts: { folder?: string; to_folder: string },
    o?: RequestOptions
  ): Promise<{ ok: boolean; uid: number; moved_to: string }> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/messages/${uid}/move`,
      { ...o, method: 'POST', body: { folder: opts.folder ?? 'INBOX', to_folder: opts.to_folder } }
    );
  },

  /**
   * DELETE /api/accounts/{id}/messages/{uid}?folder=X
   * Permanently deletes a message.
   */
  messageDelete(
    accountId: string,
    uid: number,
    folder = 'INBOX',
    o?: RequestOptions
  ): Promise<{ ok: boolean; uid: number; deleted_from: string }> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/messages/${uid}`,
      { ...o, method: 'DELETE', query: { folder } }
    );
  },

  /**
   * GET /api/accounts/{id}/search?q=...&folder=...
   * Account-scoped IMAP search.
   */
  searchMessages(
    accountId: string,
    q: string,
    folder = 'INBOX',
    limit = 50,
    o?: RequestOptions
  ): Promise<SearchResponse> {
    return request(`/accounts/${encodeURIComponent(accountId)}/search`, {
      ...o,
      query: { q, folder, limit }
    });
  },

  /**
   * POST /api/accounts/{id}/compose
   * Queue a new outbound message. Returns ComposeResponse with cooldown info.
   */
  compose(
    accountId: string,
    body: ComposeBody,
    o?: RequestOptions
  ): Promise<ComposeResponse> {
    return request(`/accounts/${encodeURIComponent(accountId)}/compose`, {
      ...o,
      method: 'POST',
      body
    });
  },

  /**
   * POST /api/accounts/{id}/compose/reply
   * Queue a reply or reply-all. Returns ComposeResponse with cooldown info.
   */
  composeReply(
    accountId: string,
    body: ReplyBody,
    o?: RequestOptions
  ): Promise<ComposeResponse> {
    return request(`/accounts/${encodeURIComponent(accountId)}/compose/reply`, {
      ...o,
      method: 'POST',
      body
    });
  },

  /**
   * POST /api/accounts/{id}/drafts/{draftId}/edit
   * Save operator edits. `expected_revision` is the revision the operator was
   * shown — a concurrent change returns 409 rather than clobbering it. Editing
   * clears any existing human-approval attestation server-side, so an edited
   * draft can never ride an earlier approval.
   */
  editDraft(
    accountId: string,
    draftId: string,
    body: DraftEditBody,
    o?: RequestOptions
  ): Promise<DraftEditResponse> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/drafts/${encodeURIComponent(draftId)}/edit`,
      { ...o, method: 'POST', body }
    );
  },

  /**
   * POST /api/accounts/{id}/drafts/{draftId}/send
   * Queue an approved draft into the outbox cooldown. Requires an explicit
   * `confirm: true` and the reviewed `expected_revision`; the approval
   * attestation is bound to that exact revision.
   */
  sendDraft(
    accountId: string,
    draftId: string,
    body: DraftSendBody,
    o?: RequestOptions
  ): Promise<DraftQueuedResponse> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/drafts/${encodeURIComponent(draftId)}/send`,
      { ...o, method: 'POST', body }
    );
  },

  /**
   * POST /api/accounts/{id}/drafts/{draftId}/discard
   * Discard a queued draft (undo send).
   */
  discardDraft(
    accountId: string,
    draftId: string,
    o?: RequestOptions
  ): Promise<{ draft_id: string; status: string }> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/drafts/${encodeURIComponent(draftId)}/discard`,
      { ...o, method: 'POST', body: {} }
    );
  }
};

// ── Additional domain types ───────────────────────────────────────────

export interface FolderStats {
  folder: string;
  exists: number;
  recent: number;
  unseen: number | null;
  virtual?: boolean;
}

export interface FoldersResponse {
  folders: FolderStats[];
  snoozed_virtual: FolderStats & { virtual: true };
  error?: string;
}

export interface SnoozedItem {
  id: string;
  account_id: string;
  uid: number;
  message_id: string | null;
  subject: string | null;
  from_addr: string | null;
  snoozed_folder: string;
  original_folder: string;
  snooze_until: string;
  created_at: string;
}

export interface SnoozedResponse {
  snoozed: SnoozedItem[];
}

export interface SearchMessageSummary extends MessageSummary {
  unread: boolean;
}

export interface SearchResponse {
  messages: SearchMessageSummary[];
  query: string;
}

// ── Bulk client ───────────────────────────────────────────────────────

export type BulkOp =
  | { type: 'flags'; add?: string[]; remove?: string[]; folder?: string }
  | { type: 'move'; to_folder: string; folder?: string }
  | { type: 'delete'; folder?: string };

export interface BulkItem {
  accountId: string;
  uid: number;
}

export interface BulkProgress {
  done: number;
  total: number;
  failed: Array<{ item: BulkItem; error: string }>;
}

/**
 * Client-side bulk operation runner — the fallback until a server bulk endpoint
 * ships (a Rust agent is building /messages/bulk separately; do NOT call it yet).
 *
 * - Runs `op` against each item, concurrency-capped at 4 in-flight at once.
 * - Calls `onProgress` after each item completes (pass or fail).
 * - Returns the final `BulkProgress` with `failed` entries for partial-failure UX.
 * - Accepts injectable `fetchImpl` so tests can mock at the request level.
 */
export async function bulkClient(
  op: BulkOp,
  items: BulkItem[],
  onProgress?: (progress: BulkProgress) => void,
  fetchImpl?: typeof fetch
): Promise<BulkProgress> {
  const CONCURRENCY = 4;
  const progress: BulkProgress = { done: 0, total: items.length, failed: [] };

  async function runOne(item: BulkItem): Promise<void> {
    try {
      const o: RequestOptions = fetchImpl ? { fetchImpl } : {};
      if (op.type === 'flags') {
        await request(
          `/accounts/${encodeURIComponent(item.accountId)}/messages/${item.uid}/flags`,
          { ...o, method: 'POST', body: { folder: op.folder ?? 'INBOX', add: op.add ?? [], remove: op.remove ?? [] } }
        );
      } else if (op.type === 'move') {
        await request(
          `/accounts/${encodeURIComponent(item.accountId)}/messages/${item.uid}/move`,
          { ...o, method: 'POST', body: { folder: op.folder ?? 'INBOX', to_folder: op.to_folder } }
        );
      } else if (op.type === 'delete') {
        await request(
          `/accounts/${encodeURIComponent(item.accountId)}/messages/${item.uid}`,
          { ...o, method: 'DELETE', query: { folder: op.folder ?? 'INBOX' } }
        );
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      progress.failed.push({ item, error: msg });
    }
    progress.done += 1;
    onProgress?.({ ...progress, failed: [...progress.failed] });
  }

  // Concurrency-cap via a sliding pool.
  const queue = [...items];
  const inFlight = new Set<Promise<void>>();
  while (queue.length > 0 || inFlight.size > 0) {
    while (queue.length > 0 && inFlight.size < CONCURRENCY) {
      const item = queue.shift()!;
      const p = runOne(item).finally(() => inFlight.delete(p));
      inFlight.add(p);
    }
    if (inFlight.size > 0) {
      await Promise.race(inFlight);
    }
  }

  return progress;
}

// ── Account-health derivation (shared by rail + drawer) ───────────────

export type AccountHealth = 'healthy' | 'unhealthy' | 'unknown';

/**
 * Derive a per-account health verdict from the global cockpit payload.
 *
 * Unhealthy when the account has any recorded failed-auth entry (IMAP/SMTP
 * credential rejected) or any failed action. Everything else is healthy. The
 * cockpit read is aggregate/read-only — it never probes live auth — so this is
 * a "last known state" badge, honest about being derived from history.
 */
export function accountHealthFromCockpit(
  cockpit: CockpitResponse | null,
  accountId: string
): AccountHealth {
  if (!cockpit) return 'unknown';
  const authItems = cockpit.auth?.items ?? [];
  const failedActions = cockpit.actions?.failed ?? [];
  const hasFailedAuth = authItems.some((it) => it.account_id === accountId);
  const hasFailedAction = failedActions.some((it) => it.account_id === accountId);
  return hasFailedAuth || hasFailedAction ? 'unhealthy' : 'healthy';
}
