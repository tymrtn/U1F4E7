// Tests for the v2 Agent Cockpit components + the cockpit-api draft-action path.
//
// Coverage:
//  AgentCard            — renders name, envtok_ prefix, activity count, ceiling
//  ApprovalRow          — Approve fires the callback with the draft
//  GovernorVerdictBadge — verdict → Badge variant (allow/review/block)
//  WatchPanel           — watch + route health rendering, dead-letter badge
//  cockpit page         — Approve is described as review-only, not a send
//  cockpitApi.draftAction — POSTs to the exact per-account draft endpoint

import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// vi.mock is hoisted above imports; the cockpit-api spy set lives in vi.hoisted().
const { cockpitMock } = vi.hoisted(() => ({
  cockpitMock: {
    agents: vi.fn(),
    scheduled: vi.fn(),
    watches: vi.fn(),
    draftAction: vi.fn()
  }
}));

vi.mock('$lib/cockpit-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/cockpit-api')>();
  return { ...actual, cockpitApi: cockpitMock };
});

import AgentCard from './AgentCard.svelte';
import ApprovalRow from './ApprovalRow.svelte';
import GovernorVerdictBadge from './GovernorVerdictBadge.svelte';
import WatchPanel from './WatchPanel.svelte';
import CockpitPage from '../../routes/cockpit/+page.svelte';
import {
  type AgentCard as AgentCardT,
  type AgentsResponse,
  type ApprovalDraft,
  type ScheduledResponse,
  type WatchesResponse
} from '$lib/cockpit-api';
import { resetCsrf } from '$lib/api';

const age = (iso: string | null) => (iso ? 'recently' : 'never');

const agentFixture: AgentCardT = {
  id: 'agent-1',
  name: 'skippy',
  token_prefix: 'envtok_1a2b3c4d',
  created_at: '2026-07-01T00:00:00Z',
  revoked_at: null,
  last_used_at: '2026-07-08T00:00:00Z',
  status: 'active',
  activity: { action_count: 3, event_count: 4, last_activity_at: '2026-07-08T00:00:00Z' },
  policy: {
    send_mode_ceiling: 'draft-only',
    accounts: 'all',
    folders: 'all',
    actions: 'all',
    recipients: 'restricted'
  }
};

const draftFixture: ApprovalDraft = {
  id: 'draft-9',
  account_id: 'acc1',
  subject: 'Review this reply',
  status: 'pending_review',
  created_by: 'mcp',
  created_at: '2026-07-08T00:00:00Z',
  updated_at: '2026-07-08T00:01:00Z',
  send_after: null,
  revision: 0,
  action_base: '/api/accounts/acc1/drafts/draft-9'
};

describe('AgentCard', () => {
  it('renders name, token prefix, total activity, and send ceiling', () => {
    render(AgentCard, { agent: agentFixture, age });
    expect(screen.getByText('skippy')).toBeTruthy();
    expect(screen.getByText('envtok_1a2b3c4d')).toBeTruthy();
    // action_count + event_count = 7
    expect(screen.getByText('7')).toBeTruthy();
    expect(screen.getByText('draft-only')).toBeTruthy();
  });
});

describe('ApprovalRow', () => {
  it('fires onapprove with the draft when Approve is clicked', async () => {
    const onapprove = vi.fn();
    render(ApprovalRow, {
      draft: draftFixture,
      age,
      onapprove,
      onedit: vi.fn(),
      ondiscard: vi.fn()
    });
    await fireEvent.click(screen.getByText('Approve'));
    expect(onapprove).toHaveBeenCalledWith(draftFixture);
  });

  it('renders the subject and source chip', () => {
    render(ApprovalRow, {
      draft: draftFixture,
      age,
      onapprove: vi.fn(),
      onedit: vi.fn(),
      ondiscard: vi.fn()
    });
    expect(screen.getByText('Review this reply')).toBeTruthy();
    expect(screen.getByText('mcp')).toBeTruthy();
  });
});

describe('GovernorVerdictBadge', () => {
  it.each([
    ['allow', 'env-badge-ok'],
    ['review', 'env-badge-pending'],
    ['block', 'env-badge-danger']
  ] as const)('maps %s verdict to %s', (verdict, cls) => {
    const { container } = render(GovernorVerdictBadge, { verdict, decision: verdict });
    const badge = container.querySelector('.env-badge');
    expect(badge?.classList.contains(cls)).toBe(true);
  });
});

describe('WatchPanel', () => {
  const watches: WatchesResponse = {
    watches: [
      {
        id: 'w1',
        account_id: 'acc1',
        folder: 'INBOX',
        status: 'running',
        schedule: 'foreground',
        last_heartbeat_at: '2026-07-08T00:00:00Z',
        last_event_at: '2026-07-08T00:00:00Z',
        failure_reason: null,
        health: 'ok'
      }
    ],
    routes: [
      {
        id: 'r1',
        account_id: 'acc1',
        match_expr: '{}',
        enabled: true,
        priority: 100,
        secret_prefix: 'evrt_1a2b3',
        deliveries: { delivered: 5, pending: 1, dead: 2 },
        health: 'danger',
        created_at: '2026-07-01T00:00:00Z',
        updated_at: '2026-07-01T00:00:00Z'
      }
    ],
    summary: { watches: 1, routes: 1, dead_letter: 2 }
  };

  it('renders watch folder, route secret prefix, and dead-letter badge', () => {
    render(WatchPanel, { data: watches, age });
    expect(screen.getByText('INBOX')).toBeTruthy();
    expect(screen.getByText('running')).toBeTruthy();
    expect(screen.getByText('evrt_1a2b3…')).toBeTruthy();
    expect(screen.getByText('2 dead-letter')).toBeTruthy();
  });
});

describe('cockpit approval queue', () => {
  const approvalQueue: AgentsResponse = {
    agents: [agentFixture],
    summary: { agents: 1, active_agents: 1, awaiting_approval: 1 },
    approval_queue: [{ source: 'mcp', count: 1, drafts: [draftFixture] }]
  };
  const noScheduled: ScheduledResponse = {
    account_status: 'all',
    scheduled: [],
    summary: { scheduled: 0, due: 0 },
    generated_at: '2026-08-24T09:00:00Z'
  };
  const noWatches: WatchesResponse = {
    watches: [],
    routes: [],
    summary: { watches: 0, routes: 0, dead_letter: 0 }
  };

  beforeEach(() => {
    cockpitMock.agents.mockResolvedValue(approvalQueue);
    cockpitMock.scheduled.mockResolvedValue(noScheduled);
    cockpitMock.watches.mockResolvedValue(noWatches);
  });
  afterEach(() => vi.clearAllMocks());

  it('says plainly that Approve neither sends nor exempts a later agent send', async () => {
    render(CockpitPage);

    // The queue's Approve button is the control this copy governs.
    await waitFor(() => expect(screen.getByRole('button', { name: 'Approve' })).toBeTruthy());
    const note = document.getElementById('approval-note');
    expect(note).toBeTruthy();
    // Approving is a review decision, and the operator has to be able to read
    // that off the screen: it does not transmit, it does not remove Governor
    // from an agent's later send, and the send action has its own name.
    expect(note).toHaveTextContent(/does not send/i);
    expect(note).toHaveTextContent(/governor/i);
    expect(note).toHaveTextContent(/human-only send/i);
  });
});

describe('cockpitApi.draftAction', () => {
  beforeEach(() => resetCsrf());
  afterEach(() => vi.restoreAllMocks());

  it('POSTs the approve action to the exact per-account draft endpoint', async () => {
    const { cockpitApi } = await vi.importActual<typeof import('$lib/cockpit-api')>(
      '$lib/cockpit-api'
    );
    const calls: Array<{ url: string; method?: string }> = [];
    const fetchImpl = vi.fn(async (url: string, init?: RequestInit) => {
      calls.push({ url: String(url), method: init?.method });
      // First call primes CSRF (GET /api/csrf), then the POST.
      if (String(url).endsWith('/api/csrf')) {
        return { ok: true, status: 200, json: async () => ({ token: 't' }), clone() { return this; } } as unknown as Response;
      }
      return { ok: true, status: 200, json: async () => ({ ok: true }), clone() { return this; } } as unknown as Response;
    });

    await cockpitApi.draftAction('/api/accounts/acc1/drafts/draft-9', 'approve', undefined, {
      fetchImpl: fetchImpl as unknown as typeof fetch
    });

    const post = calls.find((c) => c.method === 'POST');
    expect(post?.url).toBe('/api/accounts/acc1/drafts/draft-9/approve');
  });
});
