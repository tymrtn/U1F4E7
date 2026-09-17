// Rules page — Agents section (moved here from the deleted Cockpit, 2026-09-17).
//
// Agent identities are policy: who holds a token, what send ceiling it has,
// how tight its scope is. That is administration you do once, so it lives on
// Rules beside the per-account rule table, as read-only cards. The cards are
// global (an agent may span accounts) and must not depend on the account
// switcher.
//
// Drives the real api client with a stubbed global fetch, matching
// app-shell.test.ts.

import { render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import RulesPage from '../../routes/rules/+page.svelte';
import type { AgentsResponse } from '$lib/agents-api';

function jsonResponse(body: unknown, init: { status?: number } = {}): Response {
  const status = init.status ?? 200;
  const payload = JSON.stringify(body);
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => JSON.parse(payload),
    clone() {
      return jsonResponse(body, init);
    }
  } as unknown as Response;
}

const AGENTS: AgentsResponse = {
  agents: [
    {
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
    },
    {
      id: 'agent-2',
      name: 'old-bot',
      token_prefix: 'envtok_dead0000',
      created_at: '2026-06-01T00:00:00Z',
      revoked_at: '2026-06-30T00:00:00Z',
      last_used_at: null,
      status: 'revoked',
      activity: { action_count: 0, event_count: 0, last_activity_at: null },
      policy: {
        send_mode_ceiling: 'draft-only',
        accounts: 'all',
        folders: 'all',
        actions: 'all',
        recipients: 'all'
      }
    }
  ],
  approval_queue: [],
  summary: { agents: 2, active_agents: 1, awaiting_approval: 0 }
};

function stubFetch(agents: AgentsResponse | null) {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: RequestInfo | URL) => {
      const u = String(url);
      if (u.includes('/api/agents'))
        return agents ? jsonResponse(agents) : jsonResponse({ error: 'boom', code: 'store_error' }, { status: 500 });
      if (u.includes('/rules')) return jsonResponse({ rules: [] });
      if (u.includes('/folders')) return jsonResponse({ folders: [] });
      if (u.includes('/api/accounts'))
        return jsonResponse({
          accounts: [{ id: 'acc1', name: 'work@example.com', display_name: 'Work', username: 'work@example.com' }]
        });
      return jsonResponse({}, { status: 404 });
    })
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('Rules page — Agents section', () => {
  it('renders one card per agent with the active count', async () => {
    stubFetch(AGENTS);
    const { container } = render(RulesPage);

    const section = await waitFor(() => {
      const el = container.querySelector('#rules-agents');
      expect(el).not.toBeNull();
      return el!;
    });
    expect(section.querySelector('h2')?.textContent).toMatch(/Agents/);
    await waitFor(() => expect(section.querySelectorAll('.agent-card')).toHaveLength(2));
    expect(section.textContent).toContain('1 active');
    expect(screen.getByText('skippy')).toBeTruthy();
    expect(screen.getByText('envtok_1a2b3c4d')).toBeTruthy();
    expect(screen.getByText('old-bot')).toBeTruthy();
  });

  it('names the CLI commands that mint and revoke, since the cards are read-only', async () => {
    stubFetch(AGENTS);
    const { container } = render(RulesPage);

    const section = await waitFor(() => container.querySelector('#rules-agents')!);
    await waitFor(() => expect(section.querySelectorAll('.agent-card')).toHaveLength(2));
    expect(section.textContent).toContain('envelope agent create');
    expect(section.textContent).toContain('envelope agent revoke');
    expect(section.querySelectorAll('button')).toHaveLength(0);
  });

  it('shows an honest empty state when no agent has been minted', async () => {
    stubFetch({ agents: [], approval_queue: [], summary: { agents: 0, active_agents: 0, awaiting_approval: 0 } });
    const { container } = render(RulesPage);

    const section = await waitFor(() => container.querySelector('#rules-agents')!);
    await waitFor(() => expect(section.textContent).toContain('No agents registered'));
    expect(section.querySelectorAll('.agent-card')).toHaveLength(0);
  });

  it('reports a failed agents load in place instead of hiding the section', async () => {
    stubFetch(null);
    const { container } = render(RulesPage);

    const section = await waitFor(() => container.querySelector('#rules-agents')!);
    const alert = await waitFor(() => {
      const el = section.querySelector('[role="alert"]');
      expect(el).not.toBeNull();
      return el!;
    });
    expect(alert.textContent).toContain('store_error');
  });
});
