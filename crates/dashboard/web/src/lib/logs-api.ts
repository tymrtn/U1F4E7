// Typed client for `GET /api/events` — the Logs page. Reuses the CSRF-aware
// `request()` core from api.ts — imported, never re-implemented — following
// the review-api.ts convention of one sibling client per surface.
//
// Types are hand-written from crates/dashboard/src/handlers/events_log.rs.

import { request, type RequestOptions } from './api';

/**
 * One entry: the newest event of a same-day run of the same event about the
 * same draft or message, with `repeat_count` rows collapsed behind it.
 */
export interface LogEntry {
  id: string;
  account_id: string;
  account_label: string;
  event_type: string;
  /** Newest row in the run. */
  created_at: string;
  /** Oldest row in the run; equals `created_at` when `repeat_count` is 1. */
  first_at: string;
  repeat_count: number;
  /** Server-side collapse key; a later page can continue an earlier entry. */
  group_key: string;
  agent_id: string | null;
  acked: boolean;
  draft_id: string | null;
  draft_link: string | null;
  draft_subject: string | null;
  surface: string | null;
  decision: string | null;
  block_code: string | null;
  block_reason: string | null;
  attrs: string[];
  policy_mode: string | null;
  denial_code: string | null;
  message_id: string | null;
  folder: string;
  uid: number | null;
  message_link: string | null;
  from_addr: string | null;
  subject: string | null;
}

export interface LogsResponse {
  entries: LogEntry[];
  /** Cursor for the next page, or null when this was the last page. */
  next_before: string | null;
  limit: number;
}

export interface LogsQuery {
  account?: string | null;
  type?: string | null;
  since?: string | null;
  before?: string | null;
  limit?: number;
}

export const logsApi = {
  list(q: LogsQuery = {}, o?: RequestOptions): Promise<LogsResponse> {
    const params = new URLSearchParams();
    if (q.account) params.set('account', q.account);
    if (q.type) params.set('type', q.type);
    if (q.since) params.set('since', q.since);
    if (q.before) params.set('before', q.before);
    if (q.limit) params.set('limit', String(q.limit));
    const qs = params.toString();
    return request(`/events${qs ? `?${qs}` : ''}`, o);
  }
};
