// The rail must say which mailbox the open message belongs to.
//
// The unified inbox is a flat merge of every account, so opening a message from
// the list left the rail highlighting "Unified Inbox" and nothing else — the
// account the message actually lives in was invisible, and the only way to find
// out was to read the row's account chip before clicking.
//
// Drives the real api client with a stubbed global fetch, matching
// app-shell.test.ts.

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

const ACCOUNTS = {
  accounts: [
    { id: 'acc-work', name: 'work@example.com', display_name: 'Work', username: 'work@example.com' },
    { id: 'acc-home', name: 'home@example.com', display_name: 'Home', username: 'home@example.com' }
  ]
};

function stubFetch() {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: RequestInfo | URL) => {
      const u = String(url);
      if (u.includes('/api/agents'))
        return jsonResponse({ agents: [], approval_queue: [], summary: { agents: 0, active_agents: 0, awaiting_approval: 3 } });
      if (u.includes('/api/accounts')) return jsonResponse(ACCOUNTS);
      if (u.includes('/api/cockpit')) return jsonResponse({ accounts: [] });
      if (u.includes('/api/stats')) return jsonResponse({ drafts: 0, snoozed: 0 });
      return jsonResponse({}, { status: 404 });
    })
  );
}

function railAccount(container: HTMLElement, id: string): HTMLElement | null {
  return container.querySelector(`.rail-account[data-account-id="${id}"]`);
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('Rail — owning mailbox of the open message', () => {
  it('marks the account that owns the open message, and only that one', async () => {
    stubFetch();
    const { container } = render(Rail, { props: { activeAccountId: 'acc-home' } });

    await waitFor(() => expect(railAccount(container, 'acc-home')).not.toBeNull());

    const home = railAccount(container, 'acc-home')!;
    const work = railAccount(container, 'acc-work')!;
    expect(home.classList.contains('is-active')).toBe(true);
    expect(work.classList.contains('is-active')).toBe(false);
    // Exposed to assistive tech, not just colour.
    expect(home.getAttribute('aria-current')).toBe('true');
    expect(work.getAttribute('aria-current')).toBeNull();
  });

  it('marks nothing when no message is open', async () => {
    stubFetch();
    const { container } = render(Rail, { props: { activeAccountId: null } });

    await waitFor(() => expect(railAccount(container, 'acc-home')).not.toBeNull());

    expect(container.querySelectorAll('.rail-account.is-active')).toHaveLength(0);
  });

  /// An id that matches no loaded account must not mark a random row.
  it('marks nothing when the account is unknown to the rail', async () => {
    stubFetch();
    const { container } = render(Rail, { props: { activeAccountId: 'acc-deleted' } });

    await waitFor(() => expect(railAccount(container, 'acc-home')).not.toBeNull());

    expect(container.querySelectorAll('.rail-account.is-active')).toHaveLength(0);
  });

  it('labels the owning row in words, not colour alone', async () => {
    stubFetch();
    render(Rail, { props: { activeAccountId: 'acc-work' } });

    await waitFor(() => expect(screen.getByText('open here')).toBeTruthy());
  });

  it('badges Approvals with the awaiting-approval count from the fleet aggregate', async () => {
    stubFetch();
    render(Rail, { props: { activeAccountId: null } });
    // The rail's Approvals place lights up with the count a human is owed.
    await waitFor(() =>
      expect(screen.getByLabelText('3 awaiting approval')).toBeInTheDocument()
    );
    expect(screen.getByLabelText('3 awaiting approval').textContent).toBe('3');
  });
});
