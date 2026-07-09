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

export interface MessageDetail {
  uid: number;
  message_id: string | null;
  from_addr: string;
  to_addr: string;
  subject: string;
  date: string | null;
  flags: string[];
  text_content?: string | null;
  html_content?: string | null;
  attachments?: unknown[];
}

export interface MessageDetailResponse {
  message: MessageDetail;
}

export type DraftStatus = 'draft' | 'pending_review' | 'blocked' | 'sent' | 'discarded';

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
}

export interface DraftsResponse {
  drafts: Draft[];
}

export interface DraftResponse {
  draft: Draft;
  dashboard_path?: string;
  dashboard_url?: string;
}

/** Agent Cockpit aggregate — shape is broad; keep loose until panes land. */
export interface CockpitResponse {
  [key: string]: unknown;
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
    folder: string,
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

  cockpit(accountId: string, o?: RequestOptions): Promise<CockpitResponse> {
    return request(`/accounts/${encodeURIComponent(accountId)}/cockpit`, o);
  }
};
