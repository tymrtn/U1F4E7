// Digest board (design plan rev 3, §4a): capture bucket is real data from
// the unified inbox; category sections are honest awaiting-backend states —
// nothing categorized client-side, Categorize disabled with the reason.
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    unifiedInbox: vi.fn(),
    refreshUnifiedInbox: vi.fn()
  }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: { ...actual.api, ...apiMock } };
});

import DigestPage from '../../routes/digest/+page.svelte';
import { DIGEST_SECTIONS } from '$lib/digest';

function unifiedMsg(over: Record<string, unknown>) {
  return {
    uid: 1,
    subject: 'subject',
    from_addr: 'sender@example.com',
    date: '2026-08-28T09:00:00Z',
    message_id: '<m@x>',
    unread: false,
    account_id: 'acc-1',
    account_username: 'acc@example.com',
    account_display_name: null,
    folder: 'INBOX',
    uidvalidity: 1,
    snippet: null,
    thread_id: null,
    indexed_at: null,
    index_freshness: 'fresh',
    ...over
  };
}

const RESPONSE = {
  scope: 'unified_inbox',
  status: 'ok',
  folder: 'INBOX',
  limit: 50,
  accounts: [],
  unread_count: 7,
  freshness: 'fresh',
  messages: [
    unifiedMsg({ uid: 30, thread_id: 't1', subject: 'Renewal terms', from_addr: 'Maria Keller', unread: true }),
    unifiedMsg({ uid: 29, thread_id: 't1', subject: 'Renewal terms' }),
    unifiedMsg({ uid: 10, subject: 'Invoice 2214', from_addr: 'Rail Supply Co.' })
  ]
};

beforeEach(() => {
  apiMock.unifiedInbox.mockResolvedValue(RESPONSE);
  apiMock.refreshUnifiedInbox.mockResolvedValue({ ...RESPONSE, messages: [] });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('Digest board', () => {
  it('shows the tally from real unified data', async () => {
    render(DigestPage);
    const tally = await screen.findByTestId('digest-tally');
    expect(tally.textContent).toContain('3 messages');
    expect(tally.textContent).toContain('2 threads');
    expect(tally.textContent).toContain('7 unread');
  });

  it('renders one capture row per message (not per thread), newest first', async () => {
    // Collapsing a thread into one row would make a bulk op touch only the
    // anchor uid and silently leave the rest; the capture list is per-message.
    render(DigestPage);
    await screen.findByText('Maria Keller');
    const rows = document.querySelectorAll('.dg-row');
    expect(rows).toHaveLength(3); // uid 30, uid 29 (same thread), uid 10
    expect(rows[0].textContent).toContain('Maria Keller');
    // Selection is 1:1 with a message: three selectable checkboxes.
    expect(screen.getAllByLabelText('Select message')).toHaveLength(3);
  });

  it('reports thread count in the tally even though rows are per-message', async () => {
    render(DigestPage);
    const tally = await screen.findByTestId('digest-tally');
    expect(tally.textContent).toContain('3 messages');
    expect(tally.textContent).toContain('2 threads');
  });

  it('renders every category section as an honest awaiting-backend state', async () => {
    render(DigestPage);
    await screen.findByTestId('digest-tally');
    for (const section of DIGEST_SECTIONS) {
      expect(screen.getByText(section.label)).toBeInTheDocument();
    }
    expect(screen.getAllByText('awaiting categorize backend')).toHaveLength(DIGEST_SECTIONS.length);
    // Nothing is categorized client-side: no category section contains rows.
    for (const section of DIGEST_SECTIONS) {
      const el = document.querySelector(`[data-section="${section.key}"]`)!;
      expect(el.querySelectorAll('.dg-row')).toHaveLength(0);
    }
  });

  it('disables Categorize and says why', async () => {
    render(DigestPage);
    await screen.findByTestId('digest-tally');
    const categorize = screen.getByRole('button', { name: 'Categorize' });
    expect(categorize).toBeDisabled();
    expect(categorize.closest('[title]')?.getAttribute('title')).toMatch(/backend/i);
  });

  it('Refresh reloads through the refresh endpoint', async () => {
    render(DigestPage);
    await screen.findByTestId('digest-tally');
    await fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));
    await waitFor(() => expect(apiMock.refreshUnifiedInbox).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(screen.getByText('Nothing captured')).toBeInTheDocument());
  });

  it('surfaces a stable error code when the load fails', async () => {
    const { EnvelopeApiError } = await import('$lib/api');
    apiMock.unifiedInbox.mockRejectedValueOnce(
      new EnvelopeApiError(500, 'db_error', 'boom', null)
    );
    render(DigestPage);
    await waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
    expect(screen.getByText('db_error')).toBeInTheDocument();
  });

  it('selecting a capture row mounts the bulk toolbar', async () => {
    render(DigestPage);
    await screen.findByText('Maria Keller');
    const checkbox = screen.getAllByLabelText('Select message')[0];
    await fireEvent.click(checkbox);
    await waitFor(() =>
      expect(document.querySelector('.bulk-toolbar, [class*="bulk"]')).not.toBeNull()
    );
  });
});
