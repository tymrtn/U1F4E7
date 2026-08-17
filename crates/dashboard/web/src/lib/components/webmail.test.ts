import { render, screen, fireEvent, waitFor, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { accountHealthFromCockpit, type Account, type CockpitResponse } from '$lib/api';

// ── Module mocks ──────────────────────────────────────────────────────
// $app/* is aliased to src/test-stubs/* in vitest.config.ts (Vite must be able
// to resolve the import before any mock applies). We import the stub `page`
// and mutate it to drive route params/url.
import { page as pageState } from '$app/state';

// vi.mock is hoisted above imports; the api spy set lives in vi.hoisted().
const { apiMock, readerApiMock } = vi.hoisted(() => ({
  apiMock: {
    listAccounts: vi.fn(),
    cockpit: vi.fn(),
    stats: vi.fn(),
    unifiedInbox: vi.fn(),
    refreshUnifiedInbox: vi.fn(),
    message: vi.fn(),
    verifyAccount: vi.fn(),
    deleteAccount: vi.fn()
  },
  readerApiMock: {
    fetchMessageDetail: vi.fn(),
    fetchThread: vi.fn(),
    postFlags: vi.fn()
  }
}));

// The typed api client is mocked per-test so components exercise real
// loading/error/empty branches against controlled data.
vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: apiMock };
});

// ReaderPane uses reader-api helpers rather than api.message directly.
vi.mock('$lib/reader-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/reader-api')>();
  return {
    ...actual,
    fetchMessageDetail: readerApiMock.fetchMessageDetail,
    fetchThread: readerApiMock.fetchThread,
    postFlags: readerApiMock.postFlags
  };
});

import Rail from './Rail.svelte';
import AccountDrawer from './AccountDrawer.svelte';
import MailLayout from '../../routes/mail/[box]/+layout.svelte';
import ReaderPage from '../../routes/mail/[box]/[account]/[uid]/+page.svelte';
import { createRawSnippet } from 'svelte';

// A trivial snippet to satisfy the layout's `children` slot in tests.
const emptyChildren = createRawSnippet(() => ({ render: () => '<span></span>' }));

const HEALTHY_ACCT: Account = {
  id: 'acct-ok',
  name: 'Work',
  username: 'work@example.com',
  domain: 'example.com',
  smtp_host: 'smtp.example.com',
  smtp_port: 465,
  imap_host: 'imap.example.com',
  imap_port: 993,
  display_name: 'Work Mail'
};

const BROKEN_ACCT: Account = {
  ...HEALTHY_ACCT,
  id: 'acct-bad',
  name: 'Broken',
  username: 'broken@example.com',
  display_name: 'Broken Mail'
};

beforeEach(() => {
  pageState.params = { box: 'unified' };
  apiMock.listAccounts.mockResolvedValue({ accounts: [HEALTHY_ACCT, BROKEN_ACCT] });
  apiMock.cockpit.mockResolvedValue({
    auth: { items: [{ id: 'f1', account_id: 'acct-bad', backend: 'imap', reason: 'auth', retry_guidance: null, created_at: '2026-07-08' }] },
    actions: { failed: [] }
  } as CockpitResponse);
  apiMock.stats.mockResolvedValue({ accounts: 2, snoozed: 3, drafts: 1 });
  // ReaderPane uses reader-api helpers; wire a default no-thread response.
  readerApiMock.fetchThread.mockResolvedValue(null);
  readerApiMock.postFlags.mockResolvedValue({ ok: true, uid: 0, added: [], removed: [] });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('accountHealthFromCockpit', () => {
  const cockpit: CockpitResponse = {
    auth: { items: [{ id: 'x', account_id: 'acct-bad', backend: 'imap', reason: 'r', retry_guidance: null, created_at: 'now' }] },
    actions: { failed: [{ account_id: 'acct-fail', action_status: 'failed' }] }
  };
  it('flags accounts with a failed-auth record as unhealthy', () => {
    expect(accountHealthFromCockpit(cockpit, 'acct-bad')).toBe('unhealthy');
  });
  it('flags accounts with a failed action as unhealthy', () => {
    expect(accountHealthFromCockpit(cockpit, 'acct-fail')).toBe('unhealthy');
  });
  it('treats accounts with no failures as healthy', () => {
    expect(accountHealthFromCockpit(cockpit, 'acct-ok')).toBe('healthy');
  });
  it('returns unknown when cockpit is missing', () => {
    expect(accountHealthFromCockpit(null, 'acct-ok')).toBe('unknown');
  });
});

describe('Rail', () => {
  it('renders accounts from the mocked api with smart mailboxes', async () => {
    render(Rail);
    await waitFor(() => expect(screen.getByText('Work Mail')).toBeInTheDocument());
    expect(screen.getByText('Broken Mail')).toBeInTheDocument();
    expect(screen.getByText('Unified Inbox')).toBeInTheDocument();
    expect(screen.getByText('Snoozed')).toBeInTheDocument();
    // Snoozed count from stats.
    expect(screen.getByText('3')).toBeInTheDocument();
  });

  it('shows a Reconnect badge only on the unhealthy account row', async () => {
    render(Rail);
    await waitFor(() => expect(screen.getByText('Broken Mail')).toBeInTheDocument());
    const reconnectBadges = screen.getAllByText('Reconnect');
    // Exactly one row badge (the broken account); the healthy row has none.
    expect(reconnectBadges).toHaveLength(1);
  });

  it('surfaces a stable error code when accounts fail to load', async () => {
    const { EnvelopeApiError } = await import('$lib/api');
    apiMock.listAccounts.mockRejectedValueOnce(
      new EnvelopeApiError(500, 'db_error', 'boom', null)
    );
    render(Rail);
    await waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
    expect(screen.getByText('db_error')).toBeInTheDocument();
  });
});

describe('AccountDrawer', () => {
  it('hides Reconnect for a healthy account', () => {
    render(AccountDrawer, {
      account: HEALTHY_ACCT,
      health: 'healthy',
      open: true,
      onclose: () => {},
      onchanged: () => {}
    });
    expect(screen.queryByText('Reconnect')).not.toBeInTheDocument();
    expect(screen.getByText('Connected')).toBeInTheDocument();
  });

  it('shows Reconnect for an unhealthy account and calls verify', async () => {
    apiMock.verifyAccount.mockResolvedValue({ ok: true, imap: true, smtp: false, error: null });
    const onchanged = vi.fn();
    render(AccountDrawer, {
      account: BROKEN_ACCT,
      health: 'unhealthy',
      open: true,
      onclose: () => {},
      onchanged
    });
    const btn = screen.getByRole('button', { name: /reconnect/i });
    await fireEvent.click(btn);
    await waitFor(() => expect(apiMock.verifyAccount).toHaveBeenCalledWith('acct-bad'));
    await waitFor(() => expect(onchanged).toHaveBeenCalled());
  });

  it('requires the typed account name before delete is enabled', async () => {
    apiMock.deleteAccount.mockResolvedValue({ deleted: 'acct-ok' });
    render(AccountDrawer, {
      account: HEALTHY_ACCT,
      health: 'healthy',
      open: true,
      onclose: () => {},
      onchanged: () => {}
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Delete account' }));
    // The confirm button lives inside the modal dialog; scope to it (the drawer
    // itself is also role=dialog) so it doesn't collide with the drawer's
    // trigger button of the same label.
    const dialog = screen.getByRole('dialog', { name: 'Delete this account?' });
    const confirmBtn = within(dialog).getByRole('button', { name: /delete account/i });
    // Disabled until the exact address is typed.
    expect(confirmBtn).toBeDisabled();
    await fireEvent.click(confirmBtn);
    expect(apiMock.deleteAccount).not.toHaveBeenCalled();

    const input = screen.getByLabelText('Type the account address to confirm');
    await fireEvent.input(input, { target: { value: 'work@example.com' } });
    await waitFor(() => expect(confirmBtn).not.toBeDisabled());
    await fireEvent.click(confirmBtn);
    await waitFor(() => expect(apiMock.deleteAccount).toHaveBeenCalledWith('acct-ok'));
  });
});

describe('Unified inbox list (mail layout)', () => {
  it('renders message rows with unread styling and account chips', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox',
      status: 'ok',
      folder: 'INBOX',
      limit: 50,
      unread_count: 1,
      freshness: 'fresh',
      accounts: [],
      errors: [],
      messages: [
        {
          uid: 101,
          message_id: '<a@x>',
          from_addr: 'alice@example.com',
          to_addr: 'work@example.com',
          subject: 'Unread hello',
          date: '2026-07-08T10:00:00Z',
          flags: [],
          size: 10,
          unread: true,
          account_id: 'acct-ok',
          account_username: 'work@example.com',
          account_display_name: 'Work Mail',
          folder: 'INBOX',
          uidvalidity: 1,
          snippet: 'a short preview',
          thread_id: null,
          indexed_at: null,
          index_freshness: 'fresh'
        },
        {
          uid: 102,
          message_id: '<b@x>',
          from_addr: 'bob@example.com',
          to_addr: 'work@example.com',
          subject: 'Read already',
          date: '2026-07-07T10:00:00Z',
          flags: ['\\Seen'],
          size: 10,
          unread: false,
          account_id: 'acct-ok',
          account_username: 'work@example.com',
          account_display_name: 'Work Mail',
          folder: 'INBOX',
          uidvalidity: 1,
          snippet: null,
          thread_id: null,
          indexed_at: null,
          index_freshness: 'fresh'
        }
      ]
    });

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Unread hello')).toBeInTheDocument());

    const unreadRow = screen.getByText('Unread hello').closest('a');
    const readRow = screen.getByText('Read already').closest('a');
    expect(unreadRow?.classList.contains('is-unread')).toBe(true);
    expect(readRow?.classList.contains('is-unread')).toBe(false);

    // Row links to the deep-linkable reader URL, carrying its own folder.
    expect(unreadRow?.getAttribute('href')).toBe('/v2/mail/unified/acct-ok/101?folder=INBOX');
    // Account chip present.
    expect(screen.getAllByText('Work Mail').length).toBeGreaterThan(0);
  });

  it('renders an empty state (not an error) when the inbox is empty', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox',
      status: 'empty',
      folder: 'INBOX',
      limit: 50,
      unread_count: 0,
      freshness: 'empty',
      accounts: [],
      errors: [],
      messages: []
    });
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Inbox is empty')).toBeInTheDocument());
  });
});

describe('Reader pane', () => {
  it('loads the selected message on navigation and shows the text body', async () => {
    pageState.params = { box: 'unified', account: 'acct-ok', uid: '101' };
    pageState.url = new URL('http://localhost/v2/mail/unified/acct-ok/101') as typeof pageState.url;
    // ReaderPane calls reader-api.fetchMessageDetail, not api.message.
    readerApiMock.fetchMessageDetail.mockResolvedValueOnce({
      message: {
        uid: 101,
        message_id: '<a@x>',
        from_addr: 'alice@example.com',
        to_addr: 'work@example.com',
        to_addrs: ['work@example.com'],
        cc_addrs: [],
        subject: 'Reader subject',
        date: '2026-07-08T10:00:00Z',
        flags: [],
        text_body: 'The full plain-text body.',
        html_body: null,
        unread: true,
        attachments: []
      }
    });

    render(ReaderPage);
    await waitFor(() => expect(screen.getByText('Reader subject')).toBeInTheDocument());
    // Loaded from INBOX with the account + uid from the route.
    expect(readerApiMock.fetchMessageDetail).toHaveBeenCalledWith('acct-ok', 101, 'INBOX');
    expect(screen.getByText('The full plain-text body.')).toBeInTheDocument();
    expect(screen.getByText('alice@example.com')).toBeInTheDocument();
  });

  it('shows a stable error code when the message fails to load', async () => {
    pageState.params = { box: 'unified', account: 'acct-ok', uid: '999' };
    pageState.url = new URL('http://localhost/v2/mail/unified/acct-ok/999') as typeof pageState.url;
    const { EnvelopeApiError } = await import('$lib/api');
    readerApiMock.fetchMessageDetail.mockRejectedValueOnce(new EnvelopeApiError(502, 'imap_unavailable', 'IMAP down', null));
    render(ReaderPage);
    await waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
    expect(screen.getByText('imap_unavailable')).toBeInTheDocument();
  });
});
