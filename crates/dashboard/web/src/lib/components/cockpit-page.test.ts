// Cockpit page: "Cancel send" on a scheduled send must HOLD the draft (take it
// out of the outbox, keep it in Drafts), never discard it. The review page
// promises "Your draft is kept — throwing it away is a separate, deliberate
// action"; the cockpit has to honour the same contract.
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { cockpitMock } = vi.hoisted(() => ({
  cockpitMock: { agents: vi.fn(), scheduled: vi.fn(), watches: vi.fn(), draftAction: vi.fn() }
}));
vi.mock('$lib/cockpit-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/cockpit-api')>();
  return { ...actual, cockpitApi: { ...actual.cockpitApi, ...cockpitMock } };
});

import CockpitPage from '../../routes/cockpit/+page.svelte';

beforeEach(() => {
  cockpitMock.agents.mockResolvedValue({ agents: [], approval_queue: [], summary: { agents: 0, active_agents: 0, awaiting_approval: 0 } });
  cockpitMock.watches.mockResolvedValue({ watches: [], routes: [], summary: { watches: 0, routes: 0, dead_letter: 0 } });
  cockpitMock.scheduled.mockResolvedValue({
    scheduled: [
      {
        id: 'draft-queued',
        account_id: 'acc1',
        subject: 'Queued hello',
        created_by: 'dashboard',
        send_after: '2030-01-01T00:00:00Z',
        due: false,
        seconds_remaining: 600,
        cooldown_seconds: 120,
        governor: null,
        action_base: '/api/accounts/acc1/drafts/draft-queued'
      }
    ],
    summary: { scheduled: 1, due: 0, cooldown_seconds: 120 }
  });
  cockpitMock.draftAction.mockResolvedValue({});
});
afterEach(() => vi.clearAllMocks());

describe('Cockpit scheduled sends', () => {
  it('"Cancel send" holds the draft instead of discarding it', async () => {
    render(CockpitPage);
    await waitFor(() => expect(screen.getByText('Queued hello')).toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Cancel send' }));
    await waitFor(() => expect(cockpitMock.draftAction).toHaveBeenCalled());
    const [base, action] = cockpitMock.draftAction.mock.calls[0];
    expect(base).toBe('/api/accounts/acc1/drafts/draft-queued');
    expect(action).toBe('hold');
    expect(cockpitMock.draftAction.mock.calls.some((c) => c[1] === 'discard')).toBe(false);
  });
});
