// The rail's Review group after the Cockpit removal (2026-09-17).
//
// "Approvals" used to deep-link to `/cockpit#approvals`, an anchor that never
// existed on that page. The approval queue lives on Review, so that is where
// the rail sends you; no rail item may point at the deleted route.
//
// Drives the real api client with a stubbed global fetch, matching
// rail-active-account.test.ts.

import { render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import Rail from './Rail.svelte';

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

function stubFetch() {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: RequestInfo | URL) => {
      const u = String(url);
      if (u.includes('/api/agents'))
        return jsonResponse({
          agents: [],
          approval_queue: [],
          summary: { agents: 0, active_agents: 0, awaiting_approval: 3 }
        });
      if (u.includes('/api/accounts')) return jsonResponse({ accounts: [] });
      if (u.includes('/api/cockpit')) return jsonResponse({ accounts: [] });
      if (u.includes('/api/stats')) return jsonResponse({ drafts: 0, snoozed: 0 });
      return jsonResponse({}, { status: 404 });
    })
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('Rail — Review group', () => {
  it('sends Approvals to the Review page and still badges the count', async () => {
    stubFetch();
    render(Rail, { props: { activeAccountId: null } });

    const approvals = await screen.findByRole('link', { name: /Approvals/ });
    // base is '/v2' in the test stub (src/test-stubs/app-paths.ts).
    expect(approvals.getAttribute('href')).toBe('/v2/review');
    await waitFor(() => expect(approvals.textContent).toContain('3'));
  });

  it('has no link to the deleted cockpit route', async () => {
    stubFetch();
    const { container } = render(Rail, { props: { activeAccountId: null } });

    await screen.findByRole('link', { name: /Approvals/ });
    expect(container.querySelector('a[href*="/cockpit"]')).toBeNull();
    expect(screen.queryByRole('link', { name: 'Cockpit' })).toBeNull();
  });
});
