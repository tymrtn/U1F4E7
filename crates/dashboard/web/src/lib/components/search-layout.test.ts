// Search behavior in the mail layout (sweep blocker #5).
//
// Contracts under test:
//  • the query is parsed to IMAP criteria before hitting the API
//  • a superseded run can never overwrite a newer one's results
//  • an account that never answers is timed out: the spinner clears and the
//    failure is said out loud instead of spinning forever
//  • the scope select narrows the fan-out to one account
//  • one message never renders twice
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { page as pageState } from '$app/state';

const { apiMock, readerApiMock } = vi.hoisted(() => ({
  apiMock: {
    listAccounts: vi.fn(),
    cockpit: vi.fn(),
    stats: vi.fn(),
    unifiedInbox: vi.fn(),
    refreshUnifiedInbox: vi.fn(),
    searchMessages: vi.fn(),
    message: vi.fn()
  },
  readerApiMock: { fetchMessageDetail: vi.fn(), fetchThread: vi.fn(), postFlags: vi.fn() }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: apiMock };
});
vi.mock('$lib/reader-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/reader-api')>();
  return { ...actual, ...readerApiMock };
});

import MailLayout from '../../routes/mail/[box]/+layout.svelte';
import { createRawSnippet } from 'svelte';

const emptyChildren = createRawSnippet(() => ({ render: () => '<span></span>' }));

const ACCT_A = { id: 'acct-a', name: 'A', username: 'a@example.com', domain: 'example.com', smtp_host: 's', smtp_port: 1, imap_host: 'i', imap_port: 1, display_name: 'A' };
const ACCT_B = { id: 'acct-b', name: 'B', username: 'b@example.com', domain: 'example.com', smtp_host: 's', smtp_port: 1, imap_host: 'i', imap_port: 1, display_name: 'B' };

const HIT = (uid: number, subject: string) => ({
  uid, message_id: `<${uid}@x>`, from_addr: 'p@example.com', to_addr: 'me@example.com',
  subject, date: '2026-08-20T10:00:00Z', flags: [], size: 10, unread: false
});

const EMPTY_UNIFIED = {
  scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50, unread_count: 0,
  freshness: 'fresh', accounts: [], errors: [], messages: []
};

function setSearchUrl(q: string) {
  pageState.params = { box: 'unified' };
  pageState.url = new URL(`http://localhost/v2/mail/unified?q=${encodeURIComponent(q)}`) as typeof pageState.url;
}

beforeEach(() => {
  apiMock.listAccounts.mockResolvedValue({ accounts: [ACCT_A, ACCT_B] });
  apiMock.cockpit.mockResolvedValue({ auth: { items: [] }, actions: { failed: [] } });
  apiMock.stats.mockResolvedValue({ accounts: 2, snoozed: 0, drafts: 0 });
  apiMock.unifiedInbox.mockResolvedValue(EMPTY_UNIFIED);
  apiMock.searchMessages.mockResolvedValue({ messages: [], query: '' });
  readerApiMock.fetchThread.mockResolvedValue(null);
});
afterEach(() => {
  vi.clearAllMocks();
  vi.useRealTimers();
});

describe('search: operator parsing reaches the API', () => {
  it('sends parsed IMAP criteria, not the raw gmail-style text', async () => {
    setSearchUrl('from:dana@acme.com is:unread');
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(apiMock.searchMessages).toHaveBeenCalled());
    const [acct, q] = apiMock.searchMessages.mock.calls[0];
    expect(['acct-a', 'acct-b']).toContain(acct);
    expect(q).toBe('FROM "dana@acme.com" UNSEEN');
  });
});

describe('search: a superseded run cannot overwrite newer results', () => {
  it('keeps the latest query results when the older run resolves later', async () => {
    let resolveOld!: (v: unknown) => void;
    apiMock.searchMessages.mockImplementation((acct: string, q: string) => {
      if (q.includes('old')) return new Promise((r) => { resolveOld = r; });
      if (acct === 'acct-a') return Promise.resolve({ messages: [HIT(2, 'new hit')], query: q });
      return Promise.resolve({ messages: [], query: q });
    });
    setSearchUrl('old');
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(apiMock.searchMessages).toHaveBeenCalled());
    setSearchUrl('newquery');
    await waitFor(() => expect(screen.getByText('new hit')).toBeInTheDocument());
    resolveOld({ messages: [HIT(1, 'stale hit')], query: 'TEXT "old"' });
    await new Promise((r) => setTimeout(r, 30));
    expect(screen.queryByText('stale hit')).not.toBeInTheDocument();
    expect(screen.getByText('new hit')).toBeInTheDocument();
  });
});

describe('search: unreachable accounts time out and are reported', () => {
  it('clears the spinner, shows the fast account, and names the failure', async () => {
    vi.useFakeTimers();
    apiMock.searchMessages.mockImplementation((acct: string) => {
      if (acct === 'acct-b') return new Promise(() => {});
      return Promise.resolve({ messages: [HIT(7, 'fast hit')], query: 'q' });
    });
    setSearchUrl('hello');
    render(MailLayout, { children: emptyChildren });
    await vi.advanceTimersByTimeAsync(30000);
    expect(screen.getByText('fast hit')).toBeInTheDocument();
    expect(screen.queryByText('Searching…')).not.toBeInTheDocument();
    const note = screen.getByRole('status');
    expect(note.textContent).toMatch(/b@example\.com|1 account/);
  });
});

describe('search: scope select narrows the fan-out', () => {
  it('searches only the chosen account', async () => {
    setSearchUrl('hello');
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByRole('option', { name: 'b@example.com' })).toBeInTheDocument());
    await fireEvent.change(screen.getByLabelText('Search scope'), { target: { value: 'acct-b' } });
    await waitFor(() => expect((screen.getByLabelText('Search scope') as HTMLSelectElement).value).toBe('acct-b'));
    apiMock.searchMessages.mockClear();
    setSearchUrl('hello again');
    await waitFor(() => expect(apiMock.searchMessages).toHaveBeenCalled());
    expect(new Set(apiMock.searchMessages.mock.calls.map((c) => c[0]))).toEqual(new Set(['acct-b']));
  });
});

describe('search: results are deduplicated', () => {
  it('renders one row when the same account+uid arrives twice', async () => {
    apiMock.searchMessages.mockImplementation((acct: string) => {
      if (acct === 'acct-a') return Promise.resolve({ messages: [HIT(9, 'dup hit'), HIT(9, 'dup hit')], query: 'q' });
      return Promise.resolve({ messages: [], query: 'q' });
    });
    setSearchUrl('dup');
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getAllByText('dup hit').length).toBeGreaterThan(0));
    expect(screen.getAllByText('dup hit')).toHaveLength(1);
  });
});
