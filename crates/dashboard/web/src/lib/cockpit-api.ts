// Typed API client for the v2 Agent Cockpit aggregate surfaces.
//
// These wrap the read-only cockpit endpoints (`/api/agents`, `/api/scheduled`,
// `/api/watches`). They reuse the CSRF-aware `request()` core from api.ts —
// imported, never re-implemented — so the composer agent stays the sole owner
// of api.ts. Draft ACTIONS (approve/edit/discard/send) are NOT here: the
// cockpit UI hits the existing per-account draft endpoints directly through the
// same `request()` core, so the CSRF token flows through unchanged.
//
// Types are hand-written from crates/dashboard/src/handlers/{agents,scheduled,
// watches}.rs. Partial by design — extend as the cockpit grows.

import { request, type RequestOptions } from './api';

// ── /api/agents ───────────────────────────────────────────────────────

export interface AgentActivity {
  action_count: number;
  event_count: number;
  last_activity_at: string | null;
}

export interface AgentPolicySummary {
  send_mode_ceiling: string;
  accounts: 'all' | 'restricted';
  folders: 'all' | 'restricted';
  actions: 'all' | 'restricted';
  recipients: 'all' | 'restricted';
}

export interface AgentCard {
  id: string;
  name: string;
  token_prefix: string;
  created_at: string;
  revoked_at: string | null;
  last_used_at: string | null;
  status: 'active' | 'revoked';
  activity: AgentActivity;
  policy: AgentPolicySummary;
}

/** A draft awaiting approval, as surfaced in the per-source approval queue. */
export interface ApprovalDraft {
  id: string;
  account_id: string;
  subject: string | null;
  status: string;
  created_by: string | null;
  created_at: string;
  updated_at: string;
  send_after: string | null;
  /**
   * The draft revision the operator is viewing. Mutating actions
   * (approve/edit/send) must echo it back as `expected_revision`; the server
   * answers 409 when the draft changed since this view.
   */
  revision: number;
  /** Per-account draft action base, e.g. `/api/accounts/{id}/drafts/{id}`. */
  action_base: string;
}

export interface ApprovalGroup {
  source: string;
  count: number;
  drafts: ApprovalDraft[];
}

export interface AgentsResponse {
  agents: AgentCard[];
  summary: { agents: number; active_agents: number; awaiting_approval: number };
  approval_queue: ApprovalGroup[];
}

// ── /api/scheduled ────────────────────────────────────────────────────

export type GovernorBucket = 'allow' | 'review' | 'block';

export interface GovernorVerdict {
  decision: string;
  allowed: boolean;
  block_code: string | null;
  verdict: GovernorBucket;
  at: string;
}

export interface ScheduledItem {
  id: string;
  account_id: string;
  subject: string | null;
  created_by: string | null;
  send_after: string | null;
  due: boolean;
  seconds_remaining: number | null;
  cooldown_seconds: number;
  governor: GovernorVerdict | null;
  action_base: string;
}

export interface ScheduledResponse {
  account_status: 'all' | 'selected' | 'not_found';
  scheduled: ScheduledItem[];
  summary: { scheduled: number; due: number; cooldown_seconds?: number };
  generated_at: string;
}

// ── /api/watches ──────────────────────────────────────────────────────

export type HealthBucket = 'ok' | 'pending' | 'danger';

export interface WatchItem {
  id: string;
  account_id: string;
  folder: string;
  status: string;
  schedule: string | null;
  last_heartbeat_at: string | null;
  last_event_at: string | null;
  failure_reason: string | null;
  health: HealthBucket;
}

export interface RouteItem {
  id: string;
  account_id: string;
  match_expr: string;
  enabled: boolean;
  priority: number;
  /** Short prefix of the signing secret only — the full key is never sent. */
  secret_prefix: string | null;
  deliveries: { delivered: number; pending: number; dead: number };
  health: HealthBucket;
  created_at: string;
  updated_at: string;
}

export interface WatchesResponse {
  watches: WatchItem[];
  routes: RouteItem[];
  summary: { watches: number; routes: number; dead_letter: number };
}

// ── Endpoint helpers ──────────────────────────────────────────────────

export const cockpitApi = {
  agents(o?: RequestOptions): Promise<AgentsResponse> {
    return request('/agents', o);
  },

  scheduled(accountId?: string, o?: RequestOptions): Promise<ScheduledResponse> {
    if (accountId) {
      return request(`/accounts/${encodeURIComponent(accountId)}/scheduled`, o);
    }
    return request('/scheduled', o);
  },

  watches(o?: RequestOptions): Promise<WatchesResponse> {
    return request('/watches', o);
  },

  /**
   * A draft action against the existing per-account draft endpoints. `actionBase`
   * comes straight from the cockpit payload (`/api/accounts/{id}/drafts/{id}`);
   * `action` is one of approve/edit/discard/block/send/hold. POST + JSON body flow
   * through the shared CSRF-aware core, matching how the mail UI acts on drafts.
   */
  draftAction(
    actionBase: string,
    action: 'approve' | 'edit' | 'discard' | 'block' | 'send' | 'hold',
    body?: unknown,
    o?: RequestOptions
  ): Promise<unknown> {
    return request(`${actionBase}/${action}`, { ...o, method: 'POST', body });
  }
};
