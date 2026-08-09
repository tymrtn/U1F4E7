// Tests for v2 message list: SelectionStore, BulkToolbar, MessageRow, SearchBar,
// star optimistic revert, and bulk client partial-failure + concurrency cap.

import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { SelectionStore } from '$lib/selection.svelte';
import { bulkClient, type BulkItem, EnvelopeApiError, resetCsrf } from '$lib/api';
import { readState, __resetReadState } from '$lib/read-state.svelte';

// ── Module mocks ──────────────────────────────────────────────────────
import { page as pageState } from '$app/state';

const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    listAccounts: vi.fn(),
    cockpit: vi.fn(),
    stats: vi.fn(),
    unifiedInbox: vi.fn(),
    message: vi.fn(),
    verifyAccount: vi.fn(),
    deleteAccount: vi.fn(),
    snoozed: vi.fn(),
    drafts: vi.fn(),
    folders: vi.fn(),
    messageFlags: vi.fn(),
    messageMove: vi.fn(),
    messageDelete: vi.fn(),
    snoozeMessage: vi.fn(),
    searchMessages: vi.fn(),
  }
}));

const { rulesApiMock } = vi.hoisted(() => ({
  rulesApiMock: { create: vi.fn() }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: apiMock };
});

vi.mock('$lib/rules-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/rules-api')>();
  return { ...actual, rulesApi: rulesApiMock };
});

vi.mock('$app/navigation', () => ({ goto: vi.fn() }));

import MailLayout from '../../routes/mail/[box]/+layout.svelte';
import BulkToolbar from './BulkToolbar.svelte';
import { createRawSnippet } from 'svelte';

const emptyChildren = createRawSnippet(() => ({ render: () => '<span></span>' }));

/** Minimal Response stub for bulkClient's injected fetchImpl. */
function jsonResponse(body: unknown, init: { status?: number } = {}): Response {
  const status = init.status ?? 200;
  const payload = JSON.stringify(body);
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => JSON.parse(payload),
    clone() { return jsonResponse(body, init); }
  } as unknown as Response;
}

beforeEach(() => {
  resetCsrf();
  __resetReadState();
  pageState.params = { box: 'unified' };
  pageState.url = new URL('http://localhost/v2/mail/unified') as typeof pageState.url;
  apiMock.listAccounts.mockResolvedValue({ accounts: [] });
  apiMock.cockpit.mockResolvedValue({ auth: { items: [] }, actions: { failed: [] } });
  apiMock.stats.mockResolvedValue({ accounts: 0, snoozed: 0, drafts: 0 });
  apiMock.unifiedInbox.mockResolvedValue({
    scope: 'unified_inbox', status: 'empty', folder: 'INBOX', limit: 50,
    unread_count: 0, freshness: 'empty', accounts: [], errors: [], messages: []
  });
  apiMock.folders.mockResolvedValue({
    folders: [],
    snoozed_virtual: { folder: 'Snoozed', exists: 0, recent: 0, unseen: null, virtual: true }
  });
});

afterEach(() => {
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});

/** A global-fetch stub that records calls and returns ok JSON (handles CSRF). */
function stubOkFetch(): ReturnType<typeof vi.fn> {
  const f = vi.fn(async (url: RequestInfo | URL) => {
    if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
    return jsonResponse({ ok: true });
  });
  vi.stubGlobal('fetch', f);
  return f;
}

// ── 1. SelectionStore — range selection ───────────────────────────────

describe('SelectionStore — range selection', () => {
  it('selects a single key on toggle', () => {
    const s = new SelectionStore();
    s.toggle('a:1');
    expect(s.isSelected('a:1')).toBe(true);
    expect(s.count).toBe(1);
  });

  it('deselects on second toggle', () => {
    const s = new SelectionStore();
    s.toggle('a:1');
    s.toggle('a:1');
    expect(s.isSelected('a:1')).toBe(false);
    expect(s.count).toBe(0);
  });

  it('selects an inclusive range on rangeSelect', () => {
    const s = new SelectionStore();
    const keys = ['a:1', 'a:2', 'a:3', 'a:4', 'a:5'];
    s.toggle('a:1');               // anchor
    s.rangeSelect('a:4', keys);    // extend to index 3
    expect(s.isSelected('a:1')).toBe(true);
    expect(s.isSelected('a:2')).toBe(true);
    expect(s.isSelected('a:3')).toBe(true);
    expect(s.isSelected('a:4')).toBe(true);
    expect(s.isSelected('a:5')).toBe(false);
  });

  it('handles reverse direction range select', () => {
    const s = new SelectionStore();
    const keys = ['a:1', 'a:2', 'a:3'];
    s.toggle('a:3');
    s.rangeSelect('a:1', keys);
    expect(['a:1', 'a:2', 'a:3'].every((k) => s.isSelected(k))).toBe(true);
  });

  it('clears all on deselectAll', () => {
    const s = new SelectionStore();
    s.toggle('a:1');
    s.toggle('a:2');
    s.deselectAll();
    expect(s.count).toBe(0);
  });

  it('keyToggle toggles the focused key', () => {
    const s = new SelectionStore();
    s.keyToggle('x:7');
    expect(s.isSelected('x:7')).toBe(true);
    s.keyToggle('x:7');
    expect(s.isSelected('x:7')).toBe(false);
  });

  it('isEmpty reflects selection state', () => {
    const s = new SelectionStore();
    expect(s.isEmpty).toBe(true);
    s.toggle('a:1');
    expect(s.isEmpty).toBe(false);
  });
});

// ── 2. BulkToolbar — hidden at 0, visible at N ────────────────────────

describe('BulkToolbar — visibility', () => {
  it('is hidden when nothing is selected', () => {
    const s = new SelectionStore();
    render(BulkToolbar, { selection: s });
    expect(screen.queryByRole('toolbar')).not.toBeInTheDocument();
  });

  it('shows with count and action buttons when rows are selected', () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    s.toggle('acct-ok:102');
    render(BulkToolbar, { selection: s });
    expect(screen.getByRole('toolbar')).toBeInTheDocument();
    expect(screen.getByText('2 selected')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /archive/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /delete/i })).toBeInTheDocument();
  });
});

// ── 2b. BulkToolbar — action surface (icons, junk split, snooze, delete) ──

describe('BulkToolbar — action surface', () => {
  // Test-only helper: unlike production (which never derives identity from a
  // selection key), these fixtures know their own key shape (`acctId:uid`)
  // and can auto-derive accountId/uid from it rather than repeating both in
  // every fixture. Explicit `accountId`/`uid` in `over` still wins.
  const idx = (
    over: Record<
      string,
      { from: string; folder?: string; message_id?: string; subject?: string; accountId?: string; uid?: number }
    >
  ) => {
    const out: Record<string, { accountId: string; uid: number; from: string; folder?: string; message_id?: string; subject?: string }> = {};
    for (const [key, v] of Object.entries(over)) {
      const [accountId, uidStr] = key.split(':');
      out[key] = { accountId, uid: Number(uidStr), ...v };
    }
    return out;
  };

  /** Bare identity index for fixtures that only need account/uid resolution
   *  (no junk-rule/snooze context) — still test-only key parsing. */
  const identityIndex = (keys: string[]) => idx(Object.fromEntries(keys.map((k) => [k, { from: '' }])));

  it('exposes accessible names + visible labels for the primary actions and Snooze', () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s });
    // Primary row: Flag is the single primary flag control; its inverse (Unflag)
    // plus Mark read/unread and Move… live behind More — no orphaned text and no
    // contradictory Flag/Unflag twins in the row.
    for (const name of ['Archive', 'Snooze', 'Flag', 'Junk', 'Delete', 'More']) {
      const btn = screen.getByRole('button', { name });
      expect(btn).toBeInTheDocument();
      // Mobile compaction hides the label text but keeps an accessible name:
      // every primary button carries an explicit aria-label and a label span.
      expect(btn.getAttribute('aria-label')).toBeTruthy();
      expect(btn.querySelector('.bulk-label')).toBeTruthy();
    }
  });

  it('tucks Unflag, Mark read/unread and Move… into a single More menu', async () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s });
    // Collapsed by default — no orphaned secondary controls in the row.
    expect(screen.queryByRole('menuitem', { name: 'Unflag' })).not.toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'More' }));
    for (const name of ['Mark read', 'Mark unread', 'Unflag', 'Move…']) {
      expect(screen.getByRole('menuitem', { name })).toBeInTheDocument();
    }
  });

  it('disables actions while a bulk op is running (no double-dispatch)', async () => {
    const fetch = stubOkFetch();
    // Never resolve the move so the running state is observable.
    fetch.mockImplementation(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      return new Promise(() => {}) as unknown as Response;
    });
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s, messageIndex: identityIndex(['acct-ok:101']) });
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() =>
      expect((screen.getByRole('button', { name: 'Delete' }) as HTMLButtonElement).disabled).toBe(true)
    );
  });

  it('Inbox Delete dispatches a reversible canonical Trash move (no confirm dialog)', async () => {
    // From an ordinary mailbox, Delete must move to the provider-aware canonical
    // Trash (\Trash sentinel), reversibly — never the permanent hard-delete, and
    // never a confirmation ceremony.
    const fetch = stubOkFetch();
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s, folder: 'INBOX', messageIndex: identityIndex(['acct-ok:101']) });
    await fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    await waitFor(() => {
      const call = fetch.mock.calls.find(([u]) => String(u).includes('/messages/101/move'));
      expect(call).toBeTruthy();
      const body = JSON.parse(String((call![1] as RequestInit).body));
      expect(body).toMatchObject({ folder: 'INBOX', to_folder: '\\Trash' });
    });
    // No permanent-delete confirmation dialog for a reversible trash move.
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    // And the destructive hard-delete endpoint is never called from Inbox Delete.
    expect(fetch.mock.calls.some(([, init]) => (init as RequestInit)?.method === 'DELETE')).toBe(false);
  });

  it('Trash Delete confirms permanent deletion with count + account scope, then hard-deletes', async () => {
    const fetch = stubOkFetch();
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    s.toggle('acct-ok:102');
    // Source folder IS the Trash view → Delete means a permanent, confirmed
    // hard-delete.
    render(BulkToolbar, {
      selection: s,
      folder: 'Trash',
      messageIndex: identityIndex(['acct-ok:101', 'acct-ok:102'])
    });
    await fireEvent.click(screen.getByRole('button', { name: /delete permanently/i }));
    const dialog = screen.getByRole('dialog');
    expect(dialog.textContent).toMatch(/Delete 2 messages/i);
    expect(dialog.textContent).toMatch(/permanently/i);
    expect(dialog.textContent).toMatch(/1 account/i);
    // Nothing deleted until the operator confirms.
    expect(fetch.mock.calls.some(([, init]) => (init as RequestInit)?.method === 'DELETE')).toBe(false);
    await fireEvent.click(screen.getByRole('button', { name: /delete messages/i }));
    await waitFor(() =>
      expect(
        fetch.mock.calls.some(
          ([u, init]) =>
            (init as RequestInit)?.method === 'DELETE' && String(u).includes('/messages/101')
        )
      ).toBe(true)
    );
  });

  it('Trash Delete confirmation reports multi-account scope', async () => {
    const s = new SelectionStore();
    s.toggle('acct-a:1');
    s.toggle('acct-b:2');
    render(BulkToolbar, {
      selection: s,
      folder: 'Deleted Items',
      messageIndex: identityIndex(['acct-a:1', 'acct-b:2'])
    });
    await fireEvent.click(screen.getByRole('button', { name: /delete permanently/i }));
    expect(screen.getByRole('dialog').textContent).toMatch(/2 accounts/i);
  });

  it('Archive dispatches a canonical \\Archive move with the right account/folder/uid', async () => {
    const fetch = stubOkFetch();
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s, folder: 'INBOX', messageIndex: identityIndex(['acct-ok:101']) });
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() => {
      const call = fetch.mock.calls.find(([u]) => String(u).includes('/messages/101/move'));
      expect(call).toBeTruthy();
      const body = JSON.parse(String((call![1] as RequestInit).body));
      // Canonical sentinel — the backend resolves it to the account's real
      // Archive/All-Mail folder; the toolbar never sends a literal "Archive".
      expect(body).toMatchObject({ folder: 'INBOX', to_folder: '\\Archive' });
      expect(String(call![0])).toBe('/api/accounts/acct-ok/messages/101/move');
    });
  });

  it('dispatches each item with its OWN source folder, not the route folder', async () => {
    // A unified selection can span mailboxes: UID 5 lives in INBOX, UID 6 in
    // Sent. Archiving both must move each from its real folder — never assume the
    // toolbar's route folder for every item.
    const fetch = stubOkFetch();
    const s = new SelectionStore();
    s.toggle('acct-ok:5');
    s.toggle('acct-ok:6');
    render(BulkToolbar, {
      selection: s,
      folder: 'INBOX',
      messageIndex: idx({
        'acct-ok:5': { from: 'a@x', folder: 'INBOX' },
        'acct-ok:6': { from: 'b@x', folder: 'Sent' }
      })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() => {
      const c5 = fetch.mock.calls.find(([u]) => String(u).includes('/messages/5/move'));
      const c6 = fetch.mock.calls.find(([u]) => String(u).includes('/messages/6/move'));
      expect(c5 && c6).toBeTruthy();
      expect(JSON.parse(String((c5![1] as RequestInit).body)).folder).toBe('INBOX');
      expect(JSON.parse(String((c6![1] as RequestInit).body)).folder).toBe('Sent');
    });
  });

  it('Flag dispatches an add of \\Flagged', async () => {
    const fetch = stubOkFetch();
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, { selection: s, messageIndex: identityIndex(['acct-ok:101']) });
    await fireEvent.click(screen.getByRole('button', { name: 'Flag' }));
    await waitFor(() => {
      const call = fetch.mock.calls.find(([u]) => String(u).includes('/messages/101/flags'));
      expect(call).toBeTruthy();
      expect(JSON.parse(String((call![1] as RequestInit).body)).add).toEqual(['\\Flagged']);
    });
  });

  it('Junk → block sender defaults to the EXACT sender and needs an explicit confirm', async () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({ 'acct-ok:101': { from: 'Alice <alice@example.com>', folder: 'INBOX' } })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Junk' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /block sender/i }));

    // The exact sender scope is shown before anything is created.
    expect(screen.getByText('alice@example.com')).toBeInTheDocument();
    // No rule is created without an explicit operator action.
    expect(rulesApiMock.create).not.toHaveBeenCalled();

    stubOkFetch(); // the move goes through bulkClient
    rulesApiMock.create.mockResolvedValue({ rule: { id: 'r1' } });
    await fireEvent.click(screen.getByRole('button', { name: /block .*move to junk/i }));

    await waitFor(() => expect(rulesApiMock.create).toHaveBeenCalledTimes(1));
    const [acct, body] = rulesApiMock.create.mock.calls[0];
    expect(acct).toBe('acct-ok');
    // `from_exact` — a literal comparison, never `from` (which is a glob: a
    // `*@`-style domain rule would silently broaden the block).
    expect(body.match_expr).toEqual({ from_exact: 'alice@example.com' });
    expect(JSON.stringify(body.match_expr)).not.toContain('*@'); // never a domain rule
    // Canonical semantic target: the rule engine resolves \Junk to the account's
    // real Spam/Junk folder per provider at run time — not a stored literal.
    expect(body.action).toEqual({ move: '\\Junk' });
    // No sender PII in the rule name — rule executors log the fired rule's
    // name on every future hit.
    expect(body.name).not.toContain('alice@example.com');
    expect(body.name).not.toContain('@');
  });

  it('Junk → block sender never uses a glob-interpretable match for a wildcard-charactered local-part', async () => {
    // A local-part containing `*` is a valid (if unusual) email address. It
    // must be matched exactly — never reinterpreted as a domain-wide glob.
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({ 'acct-ok:101': { from: '*@example.com', folder: 'INBOX' } })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Junk' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /block sender/i }));

    stubOkFetch();
    rulesApiMock.create.mockResolvedValue({ rule: { id: 'r1' } });
    await fireEvent.click(screen.getByRole('button', { name: /block .*move to junk/i }));

    await waitFor(() => expect(rulesApiMock.create).toHaveBeenCalledTimes(1));
    const [, body] = rulesApiMock.create.mock.calls[0];
    // `from_exact`, not `from` — the literal `*` is never a glob.
    expect(body.match_expr).toEqual({ from_exact: '*@example.com' });
  });

  it('Junk → block sender creates ONE exact rule per distinct sender across accounts', async () => {
    const s = new SelectionStore();
    s.toggle('acct-a:1');
    s.toggle('acct-a:2'); // same sender as :1 → dedup
    s.toggle('acct-b:3');
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({
        'acct-a:1': { from: 'spam@x.com', folder: 'INBOX' },
        'acct-a:2': { from: 'spam@x.com', folder: 'INBOX' },
        'acct-b:3': { from: 'other@y.com', folder: 'INBOX' }
      })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Junk' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /block sender/i }));
    stubOkFetch();
    rulesApiMock.create.mockResolvedValue({ rule: { id: 'r' } });
    await fireEvent.click(screen.getByRole('button', { name: /block .*move to junk/i }));

    // spam@x.com (acct-a) + other@y.com (acct-b) = 2 rules, not 3.
    await waitFor(() => expect(rulesApiMock.create).toHaveBeenCalledTimes(2));
    const senders = rulesApiMock.create.mock.calls.map(
      (c) => (c[1].match_expr as { from_exact: string }).from_exact
    );
    expect(new Set(senders)).toEqual(new Set(['spam@x.com', 'other@y.com']));
  });

  it('Junk → block sender: rule creation failure never moves the message or reports success', async () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({ 'acct-ok:101': { from: 'alice@example.com', folder: 'INBOX' } })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Junk' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /block sender/i }));

    const fetch = stubOkFetch(); // the move endpoint would succeed if called
    rulesApiMock.create.mockRejectedValue(new Error('rule create failed'));
    await fireEvent.click(screen.getByRole('button', { name: /block .*move to junk/i }));

    await waitFor(() => expect(rulesApiMock.create).toHaveBeenCalledTimes(1));
    // A failed rule creation must never be followed by the move — the
    // compound operation is not "safe to move" without its block rule.
    expect(fetch.mock.calls.some(([u]) => String(u).includes('/messages/101/move'))).toBe(false);
    await waitFor(() =>
      expect(screen.getByText(/rule.*failed.*not moved/i)).toBeInTheDocument()
    );
    // The item stays selected — honestly retryable, not silently dropped.
    expect(s.isSelected('acct-ok:101')).toBe(true);
  });

  it('Junk → block sender: rule succeeds but the move fails — item stays selected, no false success', async () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({ 'acct-ok:101': { from: 'alice@example.com', folder: 'INBOX' } })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Junk' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /block sender/i }));

    rulesApiMock.create.mockResolvedValue({ rule: { id: 'r1' } });
    const fetch = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      if (String(url).includes('/messages/101/move')) {
        return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
      }
      return jsonResponse({ ok: true });
    });
    vi.stubGlobal('fetch', fetch);
    await fireEvent.click(screen.getByRole('button', { name: /block .*move to junk/i }));

    await waitFor(() => expect(rulesApiMock.create).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(screen.getByText(/1 rule added.*1 move failed/i)).toBeInTheDocument());
    // Not moved, so not "done" — stays selected for an honest retry.
    expect(s.isSelected('acct-ok:101')).toBe(true);
  });

  it('Junk → block sender: mixed outcomes across items are represented truthfully and retry never duplicates a rule', async () => {
    const s = new SelectionStore();
    s.toggle('acct-a:1'); // rule will fail
    s.toggle('acct-b:2'); // rule + move succeed
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({
        'acct-a:1': { from: 'bad@example.com', folder: 'INBOX' },
        'acct-b:2': { from: 'good@example.com', folder: 'INBOX' }
      })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Junk' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /block sender/i }));

    stubOkFetch();
    rulesApiMock.create.mockImplementation(async (_acct: string, body: { match_expr: { from_exact: string } }) => {
      if (body.match_expr.from_exact === 'bad@example.com') throw new Error('rule failed');
      return { rule: { id: 'r-good' } };
    });
    await fireEvent.click(screen.getByRole('button', { name: /block .*move to junk/i }));

    await waitFor(() => expect(rulesApiMock.create).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(screen.getByText(/1 moved to Junk/i)).toBeInTheDocument());
    expect(screen.getByText(/1 rule failed \(not moved\)/i)).toBeInTheDocument();

    // The succeeded item is done; the rule-failed item stays selected.
    expect(s.isSelected('acct-b:2')).toBe(false);
    expect(s.isSelected('acct-a:1')).toBe(true);

    // Retry: only the still-failing sender's rule is attempted again — the
    // already-succeeded sender's rule is never recreated.
    rulesApiMock.create.mockClear();
    rulesApiMock.create.mockResolvedValue({ rule: { id: 'r-retry' } });
    await fireEvent.click(screen.getByRole('button', { name: /retry failed/i }));
    await waitFor(() => expect(rulesApiMock.create).toHaveBeenCalledTimes(1));
    expect(rulesApiMock.create.mock.calls[0][1].match_expr).toEqual({ from_exact: 'bad@example.com' });
  });

  it('Snooze preset dispatches api.snoozeMessage with a future wall-clock return_at', async () => {
    apiMock.snoozeMessage.mockResolvedValue({ ok: true, uid: 101, return_at: 'x', snoozed_folder: 'Snoozed' });
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({ 'acct-ok:101': { from: 'a@x', folder: 'INBOX', message_id: '<a@x>', subject: 'Hi' } })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Snooze' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /tomorrow/i }));

    await waitFor(() => expect(apiMock.snoozeMessage).toHaveBeenCalledTimes(1));
    const [acct, uid, opts] = apiMock.snoozeMessage.mock.calls[0];
    expect(acct).toBe('acct-ok');
    expect(uid).toBe(101);
    // Sent as a UTC instant so the server can store UTC wall-clock.
    expect(opts.return_at).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z$/);
    expect(new Date(opts.return_at).getTime()).toBeGreaterThan(Date.now());
    expect(opts.message_id).toBe('<a@x>');
  });

  it('partial failure keeps the selection and reports "N …, M failed"', async () => {
    let n = 0;
    const fetch = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      n += 1;
      if (n === 2) return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
      return jsonResponse({ ok: true });
    });
    vi.stubGlobal('fetch', fetch);
    const s = new SelectionStore();
    s.toggle('acct-a:1');
    s.toggle('acct-b:2');
    render(BulkToolbar, { selection: s, messageIndex: identityIndex(['acct-a:1', 'acct-b:2']) });
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));

    await waitFor(() => expect(screen.getByText(/1 archived, 1 failed/i)).toBeInTheDocument());
    // Toolbar still present because selection was NOT cleared on partial failure.
    expect(screen.getByRole('toolbar')).toBeInTheDocument();
  });

  it('bulk read updates confirmed successes without forcing failed rows unread', async () => {
    const fetch = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      if (String(url).includes('/accounts/acct-b/')) {
        return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
      }
      return jsonResponse({ ok: true });
    });
    vi.stubGlobal('fetch', fetch);

    const s = new SelectionStore();
    s.toggle('acct-a:1');
    s.toggle('acct-b:2');
    // acct-a starts unread; acct-b already starts read. A failed Mark read on
    // acct-b must preserve read, not manufacture an unread rollback.
    readState.markUnread('acct-a', 'INBOX', 1);
    readState.markRead('acct-b', 'INBOX', 2);
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({
        'acct-a:1': { from: 'a@example.com', folder: 'INBOX' },
        'acct-b:2': { from: 'b@example.com', folder: 'INBOX' }
      })
    });

    await fireEvent.click(screen.getByRole('button', { name: 'More' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Mark read' }));
    await waitFor(() => expect(screen.getByText(/1 marked read, 1 failed/i)).toBeInTheDocument());

    expect(readState.isUnread('acct-a', 'INBOX', 1, true)).toBe(false);
    expect(readState.isUnread('acct-b', 'INBOX', 2, false)).toBe(false);
  });

  it('a failed Mark read on an already-read item restores read, not the opposite of the target', async () => {
    const fetch = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
    });
    vi.stubGlobal('fetch', fetch);

    const s = new SelectionStore();
    s.toggle('acct-a:1');
    readState.markRead('acct-a', 'INBOX', 1); // already read
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({ 'acct-a:1': { from: 'a@example.com', folder: 'INBOX' } })
    });

    await fireEvent.click(screen.getByRole('button', { name: 'More' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Mark read' }));
    await waitFor(() => expect(screen.getByText(/0 marked read, 1 failed/i)).toBeInTheDocument());

    // A failed Mark read must never assume the opposite of the requested
    // target (unread) — the item's actual prior state (read) is preserved.
    expect(readState.isUnread('acct-a', 'INBOX', 1, false)).toBe(false);
  });

  it('a failed Mark unread on an already-unread item restores unread, not the opposite of the target', async () => {
    const fetch = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
    });
    vi.stubGlobal('fetch', fetch);

    const s = new SelectionStore();
    s.toggle('acct-a:1');
    readState.markUnread('acct-a', 'INBOX', 1); // already unread
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({ 'acct-a:1': { from: 'a@example.com', folder: 'INBOX' } })
    });

    await fireEvent.click(screen.getByRole('button', { name: 'More' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Mark unread' }));
    await waitFor(() => expect(screen.getByText(/0 marked unread, 1 failed/i)).toBeInTheDocument());

    // A failed Mark unread must never assume the opposite of the requested
    // target (read) — the item's actual prior state (unread) is preserved.
    expect(readState.isUnread('acct-a', 'INBOX', 1, true)).toBe(true);
  });

  it('mixed partial failure: each item keeps its OWN actual prior state, not a blanket rollback', async () => {
    // 4 items: acct-a (fails) starts read, acct-b (succeeds) starts unread,
    // acct-c (fails) starts unread, acct-d (succeeds) starts read. A single
    // Mark read call must resolve every item to its own true outcome.
    const fetch = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      if (String(url).includes('/accounts/acct-a/') || String(url).includes('/accounts/acct-c/')) {
        return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
      }
      return jsonResponse({ ok: true });
    });
    vi.stubGlobal('fetch', fetch);

    const s = new SelectionStore();
    for (const k of ['acct-a:1', 'acct-b:2', 'acct-c:3', 'acct-d:4']) s.toggle(k);
    readState.markRead('acct-a', 'INBOX', 1);
    readState.markUnread('acct-b', 'INBOX', 2);
    readState.markUnread('acct-c', 'INBOX', 3);
    readState.markRead('acct-d', 'INBOX', 4);

    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({
        'acct-a:1': { from: 'a@x', folder: 'INBOX' },
        'acct-b:2': { from: 'b@x', folder: 'INBOX' },
        'acct-c:3': { from: 'c@x', folder: 'INBOX' },
        'acct-d:4': { from: 'd@x', folder: 'INBOX' }
      })
    });

    await fireEvent.click(screen.getByRole('button', { name: 'More' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Mark read' }));
    await waitFor(() => expect(screen.getByText(/2 marked read, 2 failed/i)).toBeInTheDocument());

    // Failed items (a, c) keep their own actual prior state.
    expect(readState.isUnread('acct-a', 'INBOX', 1, true)).toBe(false); // stayed read
    expect(readState.isUnread('acct-c', 'INBOX', 3, false)).toBe(true); // stayed unread
    // Succeeded items (b, d) now reflect the confirmed new state.
    expect(readState.isUnread('acct-b', 'INBOX', 2, true)).toBe(false);
    expect(readState.isUnread('acct-d', 'INBOX', 4, true)).toBe(false);
  });
});

// ── 3. bulkClient — partial failure aggregation + concurrency cap ──────
// bulkClient uses request() internally. We inject fetchImpl to mock HTTP.

describe('bulkClient — partial failure + concurrency', () => {
  it('returns all succeeded when all calls succeed', async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      return jsonResponse({ ok: true });
    });
    const items: BulkItem[] = [
      { accountId: 'a', uid: 1 },
      { accountId: 'a', uid: 2 },
    ];
    const result = await bulkClient({ type: 'flags', add: ['\\Seen'] }, items, undefined, fetchImpl);
    expect(result.total).toBe(2);
    expect(result.failed).toHaveLength(0);
    expect(result.done).toBe(2);
  });

  it('aggregates partial failures without throwing', async () => {
    let callCount = 0;
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      callCount += 1;
      if (callCount % 2 === 0) return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
      return jsonResponse({ ok: true });
    });
    const items: BulkItem[] = Array.from({ length: 4 }, (_, i) => ({ accountId: 'a', uid: i + 1 }));
    const progressCalls: { done: number; total: number }[] = [];
    const result = await bulkClient(
      { type: 'flags', add: ['\\Seen'] }, items,
      (p) => progressCalls.push({ done: p.done, total: p.total }),
      fetchImpl
    );
    expect(result.total).toBe(4);
    expect(result.failed).toHaveLength(2);
    // Progress fires for every item
    expect(progressCalls).toHaveLength(4);
    expect(progressCalls[progressCalls.length - 1].done).toBe(4);
  });

  it('caps concurrency at 4 in-flight at once', async () => {
    let maxInFlight = 0;
    let currentInFlight = 0;
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      currentInFlight += 1;
      maxInFlight = Math.max(maxInFlight, currentInFlight);
      await new Promise((r) => setTimeout(r, 10));
      currentInFlight -= 1;
      return jsonResponse({ ok: true });
    });
    // 8 items — if concurrency cap works, max in flight (excl. the shared CSRF call) is 4
    const items: BulkItem[] = Array.from({ length: 8 }, (_, i) => ({ accountId: 'a', uid: i + 1 }));
    await bulkClient({ type: 'flags', add: ['\\Seen'] }, items, undefined, fetchImpl);
    expect(maxInFlight).toBeLessThanOrEqual(4);
  });

  it('calls onProgress after each item', async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      return jsonResponse({ ok: true });
    });
    const items: BulkItem[] = [{ accountId: 'a', uid: 1 }, { accountId: 'a', uid: 2 }];
    const progress: number[] = [];
    await bulkClient({ type: 'flags', add: ['\\Seen'] }, items, (p) => progress.push(p.done), fetchImpl);
    expect(progress).toEqual([1, 2]);
  });
});

// ── 4. Star optimistic revert ─────────────────────────────────────────

describe('Star optimistic revert', () => {
  it('calls messageFlags with \\Flagged add when starring', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 1, freshness: 'fresh', accounts: [], errors: [],
      messages: [{
        uid: 101, message_id: '<a@x>', from_addr: 'alice@example.com',
        to_addr: 'work@example.com', subject: 'Hello', date: '2026-07-08T10:00:00Z',
        flags: [], size: 10, unread: true, account_id: 'acct-ok',
        account_username: 'work@example.com', account_display_name: 'Work Mail',
        folder: 'INBOX', uidvalidity: 1, snippet: null, thread_id: null,
        indexed_at: null, index_freshness: 'fresh'
      }]
    });
    apiMock.messageFlags.mockResolvedValue({ ok: true, uid: 101, added: ['\\Flagged'], removed: [] });

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Hello')).toBeInTheDocument());

    const starBtn = screen.getByRole('button', { name: /star message/i });
    await fireEvent.click(starBtn);

    await waitFor(() =>
      expect(apiMock.messageFlags).toHaveBeenCalledWith(
        'acct-ok', 101,
        expect.objectContaining({ add: ['\\Flagged'], remove: [] })
      )
    );
  });

  it('reverts star if messageFlags throws', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 1, freshness: 'fresh', accounts: [], errors: [],
      messages: [{
        uid: 101, message_id: '<a@x>', from_addr: 'alice@example.com',
        to_addr: 'work@example.com', subject: 'Hello', date: '2026-07-08T10:00:00Z',
        flags: [], size: 10, unread: true, account_id: 'acct-ok',
        account_username: 'work@example.com', account_display_name: 'Work Mail',
        folder: 'INBOX', uidvalidity: 1, snippet: null, thread_id: null,
        indexed_at: null, index_freshness: 'fresh'
      }]
    });
    apiMock.messageFlags.mockRejectedValue(new EnvelopeApiError(502, 'imap_error', 'timeout', null));

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Hello')).toBeInTheDocument());

    const starBtn = screen.getByRole('button', { name: /star message/i });
    // Before clicking: unstarred state
    expect(starBtn.textContent?.trim()).toBe('☆');
    await fireEvent.click(starBtn);

    // After the failed API call, optimistic state reverts to unstarred
    await waitFor(() => expect(starBtn.textContent?.trim()).toBe('☆'));
  });
});

// ── 4b. Read-state bridge (list ↔ reader) ─────────────────────────────

describe('Read-state bridge', () => {
  const unifiedMsg = (uid: number, subject: string, unread: boolean) => ({
    uid, message_id: `<${uid}@x>`, from_addr: 'alice@example.com',
    to_addr: 'work@example.com', subject, date: '2026-07-08T10:00:00Z',
    flags: unread ? [] : ['\\Seen'], size: 10, unread, account_id: 'acct-ok',
    account_username: 'work@example.com', account_display_name: 'Work Mail',
    folder: 'INBOX', uidvalidity: 1, snippet: null, thread_id: null,
    indexed_at: null, index_freshness: 'fresh'
  });

  it('renders an unread row bold (is-unread) and a read row normal', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 1, freshness: 'fresh', accounts: [], errors: [],
      messages: [unifiedMsg(101, 'Unread one', true), unifiedMsg(102, 'Read one', false)]
    });
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Unread one')).toBeInTheDocument());

    const rows = screen.getAllByRole('row');
    const unreadRow = rows.find((r) => r.textContent?.includes('Unread one'))!;
    const readRow = rows.find((r) => r.textContent?.includes('Read one'))!;
    expect(unreadRow.classList.contains('is-unread')).toBe(true);
    expect(readRow.classList.contains('is-unread')).toBe(false);
  });

  it('drops unread bold on the mounted row when the reader marks it read (no refetch)', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 1, freshness: 'fresh', accounts: [], errors: [],
      messages: [unifiedMsg(101, 'Bridge subject', true)]
    });
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Bridge subject')).toBeInTheDocument());

    const row = screen.getAllByRole('row').find((r) => r.textContent?.includes('Bridge subject'))!;
    expect(row.classList.contains('is-unread')).toBe(true);

    // Simulate the reader marking the message read on open. No list refetch.
    apiMock.unifiedInbox.mockClear();
    readState.markRead('acct-ok', 'INBOX', 101);

    await waitFor(() => expect(row.classList.contains('is-unread')).toBe(false));
    expect(apiMock.unifiedInbox).not.toHaveBeenCalled();
  });
});

// ── 5. Search URL persistence ─────────────────────────────────────────

describe('Search URL persistence', () => {
  it('restores box list when search query is absent from URL', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 1, freshness: 'fresh', accounts: [], errors: [],
      messages: [{
        uid: 101, message_id: '<a@x>', from_addr: 'alice@example.com',
        to_addr: 'work@example.com', subject: 'Full list message', date: '2026-07-09T10:00:00Z',
        flags: [], size: 10, unread: true, account_id: 'acct-ok',
        account_username: 'work@example.com', account_display_name: 'Work Mail',
        folder: 'INBOX', uidvalidity: 1, snippet: null, thread_id: null,
        indexed_at: null, index_freshness: 'fresh'
      }]
    });
    pageState.url = new URL('http://localhost/v2/mail/unified') as typeof pageState.url;
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Full list message')).toBeInTheDocument());
  });

  it('shows search results when ?q= is present in URL', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 0, freshness: 'fresh', accounts: [], errors: [], messages: []
    });
    apiMock.searchMessages.mockResolvedValue({
      messages: [{
        uid: 200, message_id: '<b@x>', from_addr: 'carol@example.com',
        to_addr: 'work@example.com', subject: 'Search result subject',
        date: '2026-07-09T10:00:00Z', flags: [], size: 10, unread: true
      }],
      query: 'carol'
    });
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-ok', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });

    pageState.url = new URL('http://localhost/v2/mail/unified?q=carol') as typeof pageState.url;
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Search result subject')).toBeInTheDocument());
  });
});

// ── 6. Box-specific empty states (snoozed + drafts wired) ────────────

describe('Box-specific wired boxes', () => {
  it('renders empty state for snoozed when GET /snoozed returns empty list', async () => {
    pageState.params = { box: 'snoozed' };
    pageState.url = new URL('http://localhost/v2/mail/snoozed') as typeof pageState.url;
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-ok', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });
    apiMock.snoozed.mockResolvedValue({ snoozed: [] });

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Nothing snoozed')).toBeInTheDocument());
  });

  it('renders empty state for drafts when GET /drafts returns empty list', async () => {
    pageState.params = { box: 'drafts' };
    pageState.url = new URL('http://localhost/v2/mail/drafts') as typeof pageState.url;
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-ok', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });
    apiMock.drafts.mockResolvedValue({ drafts: [] });

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('No drafts')).toBeInTheDocument());
  });
});

// ── 7. Selection identity across surfaces (B1) ─────────────────────────
//
// BulkToolbar must never derive account/folder/UID by parsing an opaque
// selection key. Drafts have no real IMAP identity and must not expose
// mailbox bulk actions at all; search and snoozed DO have a real identity
// and must dispatch against it — not a parsed "search"/"snoozed" prefix and
// not a default INBOX folder.

function stubOkFetch2(): ReturnType<typeof vi.fn> {
  const f = vi.fn(async (url: RequestInfo | URL) => {
    if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
    return jsonResponse({ ok: true });
  });
  vi.stubGlobal('fetch', f);
  return f;
}

describe('Selection identity across surfaces', () => {
  it('drafts: no mailbox bulk-action toolbar is exposed even when a row is selected', async () => {
    pageState.params = { box: 'drafts' };
    pageState.url = new URL('http://localhost/v2/mail/drafts') as typeof pageState.url;
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-ok', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });
    apiMock.drafts.mockResolvedValue({
      drafts: [{
        id: 'draft-1', account_id: 'acct-ok', status: 'draft', to_addr: 'x@y.com',
        cc_addr: null, bcc_addr: null, reply_to: null, subject: 'Draft subject',
        text_content: null, html_content: null, in_reply_to: null, metadata: null,
        attachments: [], message_id: null, send_after: null, snoozed_until: null,
        created_at: '2026-07-08T10:00:00Z', updated_at: '2026-07-08T10:00:00Z',
        sent_at: null, created_by: null, revision: 1
      }]
    });

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Draft subject')).toBeInTheDocument());

    const checkbox = screen.getByRole('checkbox');
    await fireEvent.click(checkbox);

    // Selection can still register, but no mailbox-action toolbar renders —
    // drafts have no account/folder/UID to dispatch a move/flag/junk against.
    expect(screen.queryByRole('toolbar')).not.toBeInTheDocument();
  });

  it('search: bulk Archive dispatches against the hit\'s REAL account, not a parsed "search" prefix', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 0, freshness: 'fresh', accounts: [], errors: [], messages: []
    });
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-real', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });
    apiMock.searchMessages.mockResolvedValue({
      messages: [{
        uid: 200, message_id: '<b@x>', from_addr: 'carol@example.com',
        to_addr: 'work@example.com', subject: 'Search hit subject',
        date: '2026-07-09T10:00:00Z', flags: [], size: 10, unread: true
      }],
      query: 'carol'
    });

    pageState.url = new URL('http://localhost/v2/mail/unified?q=carol') as typeof pageState.url;
    const fetch = stubOkFetch2();
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Search hit subject')).toBeInTheDocument());

    await fireEvent.click(screen.getByRole('checkbox'));
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));

    await waitFor(() => {
      const call = fetch.mock.calls.find(([u]) => String(u).includes('/messages/200/move'));
      expect(call).toBeTruthy();
      // Real account in the URL — never the literal "search" prefix or an
      // empty account id.
      expect(String(call![0])).toBe('/api/accounts/acct-real/messages/200/move');
    });
  });

  it('snoozed: bulk Archive dispatches against the snoozed_folder (its real current location)', async () => {
    pageState.params = { box: 'snoozed' };
    pageState.url = new URL('http://localhost/v2/mail/snoozed') as typeof pageState.url;
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-ok', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });
    apiMock.snoozed.mockResolvedValue({
      snoozed: [{
        id: 'snz-1', account_id: 'acct-ok', uid: 55, message_id: '<s@x>',
        subject: 'Snoozed subject', from_addr: 'dana@example.com',
        snoozed_folder: 'Snoozed', original_folder: 'INBOX',
        snooze_until: '2026-08-01T09:00:00Z', created_at: '2026-07-08T10:00:00Z'
      }]
    });

    const fetch = stubOkFetch2();
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Snoozed subject')).toBeInTheDocument());

    await fireEvent.click(screen.getByRole('checkbox'));
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));

    await waitFor(() => {
      const call = fetch.mock.calls.find(([u]) => String(u).includes('/messages/55/move'));
      expect(call).toBeTruthy();
      const body = JSON.parse(String((call![1] as RequestInit).body));
      // The message physically sits in the Snoozed folder right now — moving
      // it must reference that real current folder, not original_folder
      // (where it isn't) or a default INBOX.
      expect(body.folder).toBe('Snoozed');
      expect(String(call![0])).toBe('/api/accounts/acct-ok/messages/55/move');
    });
  });
});

// ── 8. Unresolved selection disables mailbox actions (B1) ──────────────

describe('BulkToolbar — unresolved selection disables mailbox actions', () => {
  it('disables every mailbox action while any selected key has no messageIndex entry', () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101'); // resolvable
    s.toggle('ghost-key'); // no messageIndex entry at all — unresolvable
    render(BulkToolbar, {
      selection: s,
      messageIndex: { 'acct-ok:101': { accountId: 'acct-ok', uid: 101, from: 'a@x.com' } }
    });
    for (const name of ['Archive', 'Snooze', 'Flag', 'Junk', 'Delete', 'More']) {
      expect((screen.getByRole('button', { name }) as HTMLButtonElement).disabled).toBe(true);
    }
  });

  it('an unresolved key never executes or reports a zero-item success', async () => {
    const fetch = stubOkFetch();
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    s.toggle('ghost-key');
    render(BulkToolbar, {
      selection: s,
      messageIndex: { 'acct-ok:101': { accountId: 'acct-ok', uid: 101, from: 'a@x.com' } }
    });
    // The Archive button is disabled, so a click must not dispatch anything.
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await new Promise((r) => setTimeout(r, 0));
    expect(fetch.mock.calls.some(([u]) => String(u).includes('/move'))).toBe(false);
    expect(screen.queryByText(/archived/i)).not.toBeInTheDocument();
  });

  it('re-enables mailbox actions once every selected key resolves to a real identity', () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, {
      selection: s,
      messageIndex: { 'acct-ok:101': { accountId: 'acct-ok', uid: 101, from: 'a@x.com' } }
    });
    expect((screen.getByRole('button', { name: 'Archive' }) as HTMLButtonElement).disabled).toBe(false);
  });
});

// ── 9. Block & Junk: invalid/missing sender never treated as success (B6) ──

describe('BulkToolbar — Block & Junk invalid sender guard', () => {
  const idx = (over: Record<string, { from: string; folder?: string }>) => {
    const out: Record<string, { accountId: string; uid: number; from: string; folder?: string }> = {};
    for (const [key, v] of Object.entries(over)) {
      const [accountId, uidStr] = key.split(':');
      out[key] = { accountId, uid: Number(uidStr), ...v };
    }
    return out;
  };

  it('disables the compound confirm and never creates a rule or moves when no selected item has a valid sender', async () => {
    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({ 'acct-ok:101': { from: '' } })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Junk' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /block sender/i }));

    const confirmBtn = screen.getByRole('button', { name: /block .*move to junk/i });
    expect((confirmBtn as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText(/use "move to junk" instead/i)).toBeInTheDocument();

    const fetch = stubOkFetch();
    rulesApiMock.create.mockResolvedValue({ rule: { id: 'r1' } });
    await fireEvent.click(confirmBtn);
    await new Promise((r) => setTimeout(r, 0));
    expect(rulesApiMock.create).not.toHaveBeenCalled();
    expect(fetch.mock.calls.some(([u]) => String(u).includes('/move'))).toBe(false);
    // The invalid-sender item is never silently deselected.
    expect(s.isSelected('acct-ok:101')).toBe(true);
  });

  it('disables the compound confirm when the selection mixes a valid and an invalid sender', async () => {
    const s = new SelectionStore();
    s.toggle('acct-a:1'); // no readable sender
    s.toggle('acct-b:2'); // valid sender
    render(BulkToolbar, {
      selection: s,
      messageIndex: idx({
        'acct-a:1': { from: '' },
        'acct-b:2': { from: 'good@example.com' }
      })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Junk' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /block sender/i }));

    expect(screen.getByText(/1 selected message.*no readable sender/i)).toBeInTheDocument();
    const confirmBtn = screen.getByRole('button', { name: /block .*move to junk/i });
    expect((confirmBtn as HTMLButtonElement).disabled).toBe(true);
  });
});

// ── 10. BulkToolbar stays mounted through refresh (B6) ──────────────────

describe('BulkToolbar stays mounted through onoperated refresh', () => {
  it('rule-success/move-failure leaves a visible Retry failed control through the refresh, and retry does not recreate the rule', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 1, freshness: 'fresh', accounts: [], errors: [],
      messages: [{
        uid: 101, message_id: '<a@x>', from_addr: 'alice@example.com',
        to_addr: 'work@example.com', subject: 'Hello', date: '2026-07-08T10:00:00Z',
        flags: [], size: 10, unread: true, account_id: 'acct-ok',
        account_username: 'work@example.com', account_display_name: 'Work Mail',
        folder: 'INBOX', uidvalidity: 1, snippet: null, thread_id: null,
        indexed_at: null, index_freshness: 'fresh'
      }]
    });
    rulesApiMock.create.mockResolvedValue({ rule: { id: 'r1' } });
    const fetch = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      if (String(url).includes('/messages/101/move')) {
        return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
      }
      return jsonResponse({ ok: true });
    });
    vi.stubGlobal('fetch', fetch);

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Hello')).toBeInTheDocument());

    await fireEvent.click(screen.getByRole('checkbox'));
    await fireEvent.click(screen.getByRole('button', { name: 'Junk' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: /block sender/i }));
    await fireEvent.click(screen.getByRole('button', { name: /block .*move to junk/i }));

    await waitFor(() => expect(rulesApiMock.create).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(screen.getByText(/1 rule added.*1 move failed/i)).toBeInTheDocument());

    // The compound op's onoperated triggers a real list refresh (loadUnified)
    // — the toolbar must stay mounted through it so the retry toast and
    // createdBlockRuleKeys survive rather than resetting on remount.
    await waitFor(() => expect(apiMock.unifiedInbox).toHaveBeenCalledTimes(2));
    expect(screen.getByRole('button', { name: /retry failed/i })).toBeInTheDocument();

    rulesApiMock.create.mockClear();
    await fireEvent.click(screen.getByRole('button', { name: /retry failed/i }));
    await new Promise((r) => setTimeout(r, 0));
    // The rule already succeeded before the refresh — retry must not
    // recreate it.
    expect(rulesApiMock.create).not.toHaveBeenCalled();
  });

  it('rejects a retry payload after its selected identity leaves the current view', async () => {
    const fetch = vi.fn(async (url: RequestInfo | URL) => {
      if (String(url).includes('/api/csrf')) return jsonResponse({ token: 'tok' });
      if (String(url).includes('/messages/101/move')) {
        return jsonResponse({ code: 'imap_error', error: 'timeout' }, { status: 502 });
      }
      return jsonResponse({ ok: true });
    });
    vi.stubGlobal('fetch', fetch);

    const s = new SelectionStore();
    s.toggle('acct-ok:101');
    render(BulkToolbar, {
      selection: s,
      messageIndex: {
        'acct-ok:101': { accountId: 'acct-ok', uid: 101, from: 'alice@example.com', folder: 'INBOX' }
      }
    });

    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() => expect(screen.getByRole('button', { name: /retry failed/i })).toBeInTheDocument());
    const moveCallsBefore = fetch.mock.calls.filter(([url]) => String(url).includes('/messages/101/move')).length;

    // Simulate a mailbox/search-context change clearing the old selection.
    s.clear();
    await fireEvent.click(screen.getByRole('button', { name: /retry failed/i }));

    await waitFor(() => expect(screen.getByText(/retry expired after mailbox or search changed/i)).toBeInTheDocument());
    const moveCallsAfter = fetch.mock.calls.filter(([url]) => String(url).includes('/messages/101/move')).length;
    expect(moveCallsAfter).toBe(moveCallsBefore);
  });
});

// ── 11. Selection clears on search query change (B1) ────────────────────

describe('Selection clears when the search query changes', () => {
  it('clears a stale selection so it cannot act on a new result set', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 0, freshness: 'fresh', accounts: [], errors: [], messages: []
    });
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-real', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });
    apiMock.searchMessages.mockResolvedValue({
      messages: [{
        uid: 200, message_id: '<b@x>', from_addr: 'carol@example.com',
        to_addr: 'work@example.com', subject: 'Search hit subject',
        date: '2026-07-09T10:00:00Z', flags: [], size: 10, unread: true
      }],
      query: 'carol'
    });

    pageState.url = new URL('http://localhost/v2/mail/unified?q=carol') as typeof pageState.url;
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Search hit subject')).toBeInTheDocument());

    await fireEvent.click(screen.getByRole('checkbox'));
    expect(screen.getByRole('toolbar')).toBeInTheDocument();

    // Query changes (cleared back to the full list here) — the hidden
    // selection from the old result set must not silently persist.
    pageState.url = new URL('http://localhost/v2/mail/unified') as typeof pageState.url;
    await waitFor(() => expect(screen.queryByRole('toolbar')).not.toBeInTheDocument());
  });
});

// ── 12. Search/Snoozed render unread from the shared readState override ──

describe('Search/Snoozed read-state rendering', () => {
  it('search rows render unread state from the readState override, not just the raw backend flag', async () => {
    apiMock.unifiedInbox.mockResolvedValue({
      scope: 'unified_inbox', status: 'ok', folder: 'INBOX', limit: 50,
      unread_count: 0, freshness: 'fresh', accounts: [], errors: [], messages: []
    });
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-real', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });
    apiMock.searchMessages.mockResolvedValue({
      messages: [{
        uid: 200, message_id: '<b@x>', from_addr: 'carol@example.com',
        to_addr: 'work@example.com', subject: 'Search hit subject',
        date: '2026-07-09T10:00:00Z', flags: [], size: 10, unread: true
      }],
      query: 'carol'
    });

    pageState.url = new URL('http://localhost/v2/mail/unified?q=carol') as typeof pageState.url;
    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Search hit subject')).toBeInTheDocument());

    const row = screen.getAllByRole('row').find((r) => r.textContent?.includes('Search hit subject'))!;
    expect(row.classList.contains('is-unread')).toBe(true);

    readState.markRead('acct-real', 'INBOX', 200);
    await waitFor(() => expect(row.classList.contains('is-unread')).toBe(false));
  });

  it('snoozed rows render unread state from the readState override instead of hard-coded read', async () => {
    pageState.params = { box: 'snoozed' };
    pageState.url = new URL('http://localhost/v2/mail/snoozed') as typeof pageState.url;
    apiMock.listAccounts.mockResolvedValue({
      accounts: [{
        id: 'acct-ok', name: 'Work', username: 'work@example.com',
        domain: 'example.com', smtp_host: 'smtp.example.com', smtp_port: 465,
        imap_host: 'imap.example.com', imap_port: 993
      }]
    });
    apiMock.snoozed.mockResolvedValue({
      snoozed: [{
        id: 'snz-1', account_id: 'acct-ok', uid: 55, message_id: '<s@x>',
        subject: 'Snoozed subject', from_addr: 'dana@example.com',
        snoozed_folder: 'Snoozed', original_folder: 'INBOX',
        snooze_until: '2026-08-01T09:00:00Z', created_at: '2026-07-08T10:00:00Z'
      }]
    });

    render(MailLayout, { children: emptyChildren });
    await waitFor(() => expect(screen.getByText('Snoozed subject')).toBeInTheDocument());

    const row = screen.getAllByRole('row').find((r) => r.textContent?.includes('Snoozed subject'))!;
    expect(row.classList.contains('is-unread')).toBe(false); // no override yet — base false

    readState.markUnread('acct-ok', 'Snoozed', 55);
    await waitFor(() => expect(row.classList.contains('is-unread')).toBe(true));
  });
});
