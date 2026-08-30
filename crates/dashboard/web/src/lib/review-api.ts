// Typed API client for the Review queue aggregate (`GET /api/review`).
//
// Reuses the CSRF-aware `request()` core from api.ts — imported, never
// re-implemented — following the cockpit-api.ts convention of one sibling
// client per aggregate surface. The review load is strictly read-only; acting
// on an item happens on the surface its `link` points to.
//
// Types are hand-written from crates/dashboard/src/handlers/review.rs.

import { request, type RequestOptions } from './api';

// ── Decide now ────────────────────────────────────────────────────────

/** A pending-review or blocked draft awaiting an operator decision. */
export interface ReviewDraftItem {
  id: string;
  account_id: string;
  account_label: string;
  subject: string | null;
  status: string;
  created_by: string | null;
  created_at: string;
  updated_at: string;
  send_after: string | null;
  revision: number;
  /** Canonical draft review surface, e.g. `/accounts/{id}/drafts/{id}`. */
  link: string;
  action_base: string;
}

/**
 * A terminally failed agent action (status `failed`, `error`, or `denied` —
 * the store's ACTION_FAILURE_STATUSES whitelist). In-flight and unknown
 * statuses never land here. Free-text `justification` stays server-side.
 */
export interface ReviewFailedAction {
  id: string;
  account_id: string;
  account_label: string;
  action_type: string;
  action_status: string;
  draft_id: string | null;
  draft_link: string | null;
  created_at: string;
}

/**
 * A capped item list with its true total: `count` is the whole queue,
 * `returned` how many items are present, `truncated` whether they differ.
 */
export interface CappedList<T> {
  count: number;
  returned: number;
  truncated: boolean;
  items: T[];
}

/** An unacked, non-routine event. Message-anchored ones land in needs_triage. */
export interface ReviewEventItem {
  id: string;
  account_id: string;
  account_label: string;
  event_type: string;
  outcome: string;
  from_addr: string | null;
  subject: string | null;
  folder: string;
  uid: number | null;
  message_link: string | null;
  secure_pending: boolean;
  created_at: string;
}

/** A disabled rule: a proposal awaiting review, never live automation. */
export interface ReviewProposedRule {
  id: string;
  account_id: string;
  account_label: string;
  name: string;
  action: unknown;
  review_state: 'proposed_disabled';
  live: false;
  priority: number;
  created_at: string;
  updated_at: string;
  link: string;
}

// ── Waiting ───────────────────────────────────────────────────────────

export interface ReviewScheduledItem {
  id: string;
  account_id: string;
  account_label: string;
  subject: string | null;
  created_by: string | null;
  send_after: string | null;
  due: boolean;
  seconds_remaining: number | null;
  link: string;
  action_base: string;
}

export interface ReviewSnoozeItem {
  id: string;
  account_id: string;
  account_label: string;
  subject: string | null;
  return_at: string;
  folder: string;
  uid: number;
  message_link: string;
  due: boolean;
}

// ── Operational health ────────────────────────────────────────────────

export interface ReviewFailedAuth {
  id: string;
  account_id: string;
  account_label: string;
  backend: string;
  status: 'auth_failed';
  created_at: string;
}

export interface ReviewFailedWatch {
  id: string;
  account_id: string;
  account_label: string;
  folder: string;
  status: string;
  last_heartbeat_at: string | null;
}

export interface ReviewDeadRoute {
  id: string;
  account_id: string;
  account_label: string;
  enabled: boolean;
  dead: number;
}

// ── Sent relationship history ─────────────────────────────────────────

/**
 * Fixed, truthful relationship signal derived only from aggregate topology
 * (outbound/inbound balance) and recency. Never an inferred obligation —
 * "awaiting reply" stays reserved for explicit durable snoozes.
 */
export type SentRelationshipSignal =
  | 'historical_one_way'
  | 'recent_outbound_history'
  | 'bilateral_history';

/**
 * One observed outbound counterparty for one account. The exact address is
 * the relationship identity and is carried deliberately; subjects, snippets,
 * and raw recipient headers never ride along. `link` stays null while no
 * canonical relationship surface exists — `link_state` says so instead of
 * inventing a destination.
 */
export interface SentRelationshipItem {
  counterparty: string;
  account_id: string;
  account_label: string;
  message_count: number;
  outbound_count: number;
  inbound_count: number;
  thread_count: number;
  first_observed: string | null;
  last_observed: string | null;
  signal: SentRelationshipSignal;
  link: string | null;
  link_state: 'not_available';
}

// ── Response ──────────────────────────────────────────────────────────

export interface ReviewResponse {
  summary: {
    decide_now: number;
    waiting: number;
    needs_triage: number;
    operational_health: number;
  };
  decide_now: {
    count: number;
    drafts: {
      /** True per-status totals, not capped item-list lengths. */
      counts: { pending_review: number; blocked: number };
      returned: number;
      truncated: boolean;
      items: ReviewDraftItem[];
    };
    failed_actions: CappedList<ReviewFailedAction>;
    events: CappedList<ReviewEventItem>;
    proposed_rules: { count: number; items: ReviewProposedRule[] };
  };
  waiting: {
    count: number;
    scheduled: {
      count: number;
      due: number;
      returned: number;
      truncated: boolean;
      items: ReviewScheduledItem[];
    };
    due_snoozes: { count: number; items: ReviewSnoozeItem[] };
    awaiting_reply: { count: number; items: ReviewSnoozeItem[] };
  };
  needs_triage: {
    count: number;
    returned: number;
    truncated: boolean;
    /** Truth-in-labeling: only durable store events, never a classifier. */
    source: 'durable_events';
    items: ReviewEventItem[];
  };
  operational_health: {
    count: number;
    failed_auth: CappedList<ReviewFailedAuth>;
    failed_watches: CappedList<ReviewFailedWatch>;
    dead_letters: { count: number; routes: ReviewDeadRoute[] };
  };
  /**
   * Context, not a queue: observed thread history grouped by outbound
   * counterparty, per account. Deliberately absent from `summary` — nothing
   * here needs a decision. `coverage` is the server's own provenance
   * disclosure (observed history, not a complete mailbox census).
   */
  sent_history: {
    source: 'observed_thread_history';
    coverage: string;
    count: number;
    returned: number;
    truncated: boolean;
    items: SentRelationshipItem[];
  };
  generated_at: string;
}

export const reviewApi = {
  get(o?: RequestOptions): Promise<ReviewResponse> {
    return request('/review', o);
  }
};
