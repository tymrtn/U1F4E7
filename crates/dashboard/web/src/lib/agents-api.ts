// Typed client for `/api/agents`: agent identities, their policy summary, and
// the count of drafts awaiting a human. Reuses the CSRF-aware `request()` core
// from api.ts — imported, never re-implemented — so api.ts keeps one owner.
//
// Types are hand-written from crates/dashboard/src/handlers/agents.rs and are
// partial by design — extend as consumers need more.

import { request, type RequestOptions } from './api';

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

/** A draft awaiting approval, as grouped per source in the payload. */
export interface ApprovalDraft {
  id: string;
  account_id: string;
  subject: string | null;
  status: string;
  created_by: string | null;
  created_at: string;
  updated_at: string;
  send_after: string | null;
  /** The draft revision as of this read; mutations echo it as `expected_revision`. */
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

export const agentsApi = {
  agents(o?: RequestOptions): Promise<AgentsResponse> {
    return request('/agents', o);
  }
};
