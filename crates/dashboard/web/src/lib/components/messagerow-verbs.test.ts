// MessageRow GTD verb cluster (design plan rev 3, Phase B). The row dispatches
// the same single-item ops the reader/BulkToolbar use, bumps the shared
// mailbox-ops signal on success, surfaces failures loudly, and never opens a
// new send path. Delegate is present but disabled until its backend lands.
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { bulkClientMock, snoozeMock } = vi.hoisted(() => ({
  bulkClientMock: vi.fn(),
  snoozeMock: vi.fn()
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return {
    ...actual,
    bulkClient: bulkClientMock,
    api: { ...actual.api, snoozeMessage: snoozeMock }
  };
});

import MessageRow from './MessageRow.svelte';
import { SelectionStore } from '$lib/selection.svelte';
import { getMailboxOpsStore, __resetMailboxOpsStore } from '$lib/mailbox-ops.svelte';

function mkMessage(over: Record<string, unknown> = {}) {
  return {
    key: 'acct-1:30',
    uid: 30,
    accountId: 'acct-1',
    subject: 'Renewal terms',
    from: 'Maria Keller',
    date: '2026-08-28T09:00:00Z',
    snippet: 'the revised schedule',
    unread: true,
    starred: false,
    folder: 'INBOX',
    href: '/mail/unified/acct-1/30?folder=INBOX',
    ...over
  };
}

function renderRow(over: Record<string, unknown> = {}, verbs = true) {
  const selection = new SelectionStore();
  const message = mkMessage(over);
  return render(MessageRow, {
    props: { message, selection, orderedKeys: [message.key], verbs }
  });
}

beforeEach(() => {
  __resetMailboxOpsStore();
  bulkClientMock.mockResolvedValue({ done: 1, total: 1, failed: [] });
  snoozeMock.mockResolvedValue({ ok: true, uid: 30, return_at: 'x', snoozed_folder: 'Snoozed' });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('MessageRow — identity', () => {
  it('renders a sender avatar with initials', () => {
    const { container } = renderRow();
    const avatar = container.querySelector('.avatar')!;
    expect(avatar.getAttribute('data-initials')).toBe('MK');
  });

  it('shows an unread dot and bold sender on unread rows', () => {
    const { container } = renderRow({ unread: true });
    expect(container.querySelector('.msg-unread-dot')).not.toBeNull();
    expect(container.querySelector('.msg-row')!.classList.contains('is-unread')).toBe(true);
  });

  it('tints the row by account hue only when an account chip is present', () => {
    const { container: withChip } = renderRow({ accountChip: 'work@example.com' });
    expect(withChip.querySelector('.msg-row')!.classList.contains('has-tint')).toBe(true);
    const { container: noChip } = renderRow();
    expect(noChip.querySelector('.msg-row')!.classList.contains('has-tint')).toBe(false);
  });

  it('exposes unread state to assistive tech in text, not color alone', () => {
    renderRow({ unread: true });
    // A visually-hidden "Unread." rides alongside the color dot + bold weight.
    expect(screen.getByText('Unread.')).toBeInTheDocument();
  });

  it('toggles selection from the keyboard (Space/Enter), not just the mouse', async () => {
    const selection = new SelectionStore();
    const message = mkMessage();
    render(MessageRow, {
      props: { message, selection, orderedKeys: [message.key], verbs: true }
    });
    const checkbox = screen.getByRole('checkbox', { name: 'Select message' });
    expect(selection.isSelected(message.key)).toBe(false);
    await fireEvent.keyDown(checkbox, { key: ' ' });
    expect(selection.isSelected(message.key)).toBe(true);
    await fireEvent.keyDown(checkbox, { key: 'Enter' });
    expect(selection.isSelected(message.key)).toBe(false);
  });
});

describe('MessageRow — verb cluster', () => {
  it('omits verbs when the row has no folder', () => {
    renderRow({ folder: undefined });
    expect(screen.queryByRole('button', { name: 'Archive' })).toBeNull();
  });

  it('archive dispatches a \\Archive move and bumps the ops signal', async () => {
    const ops = getMailboxOpsStore();
    const before = ops.version;
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() => expect(bulkClientMock).toHaveBeenCalledTimes(1));
    const [op, items] = bulkClientMock.mock.calls[0];
    expect(op).toEqual({ type: 'move', to_folder: '\\Archive', folder: 'INBOX' });
    expect(items).toEqual([{ accountId: 'acct-1', uid: 30, folder: 'INBOX' }]);
    await waitFor(() => expect(ops.version).toBe(before + 1));
  });

  it('delete moves to \\Trash (reversible)', async () => {
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(bulkClientMock).toHaveBeenCalledTimes(1));
    expect(bulkClientMock.mock.calls[0][0]).toEqual({
      type: 'move',
      to_folder: '\\Trash',
      folder: 'INBOX'
    });
  });

  it('delegate is present but disabled with a reason', () => {
    renderRow();
    const delegate = screen.getByRole('button', { name: 'Delegate to an agent' });
    expect(delegate).toBeDisabled();
    expect(delegate.getAttribute('title')).toMatch(/Phase E|backend/i);
  });

  it('reply is a link to the message, not a send action', () => {
    renderRow();
    const reply = screen.getByRole('link', { name: 'Reply' });
    expect(reply.getAttribute('href')).toBe('/mail/unified/acct-1/30?folder=INBOX');
  });

  it('snooze opens a menu of explicit times and dispatches the chosen one', async () => {
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'Snooze' }));
    const menu = await screen.findByRole('menu');
    expect(menu).toBeInTheDocument();
    const items = screen.getAllByRole('menuitem');
    expect(items.length).toBeGreaterThanOrEqual(2);
    await fireEvent.click(items[items.length - 1]); // "Next week"
    await waitFor(() => expect(snoozeMock).toHaveBeenCalledTimes(1));
    const [accountId, uid, opts] = snoozeMock.mock.calls[0];
    expect(accountId).toBe('acct-1');
    expect(uid).toBe(30);
    expect(opts.folder).toBe('INBOX');
    // A UTC instant (…Z), not a naive local string — the sweep compares
    // against UTC now, so a naive string would fire off by the offset.
    expect(opts.return_at).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/);
  });

  it('snooze success bumps the ops signal; failure surfaces an error without bumping', async () => {
    const ops = getMailboxOpsStore();
    // Success path.
    const before = ops.version;
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'Snooze' }));
    await fireEvent.click(screen.getAllByRole('menuitem')[0]);
    await waitFor(() => expect(snoozeMock).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(ops.version).toBe(before + 1));

    // Failure path: the endpoint rejects.
    const { EnvelopeApiError } = await import('$lib/api');
    snoozeMock.mockRejectedValueOnce(new EnvelopeApiError(502, 'imap_error', 'snooze store failed', null));
    const afterSuccess = ops.version;
    renderRow();
    const snoozeBtns = screen.getAllByRole('button', { name: 'Snooze' });
    await fireEvent.click(snoozeBtns[snoozeBtns.length - 1]);
    const items = screen.getAllByRole('menuitem');
    await fireEvent.click(items[items.length - 1]);
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toMatch(/couldn't snooze/i);
    expect(ops.version).toBe(afterSuccess);
  });

  it('surfaces a loud error when a move throws (bulkClient rejects)', async () => {
    const { EnvelopeApiError } = await import('$lib/api');
    bulkClientMock.mockRejectedValueOnce(new EnvelopeApiError(500, 'net', 'connection reset', null));
    const ops = getMailboxOpsStore();
    const before = ops.version;
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toMatch(/couldn't archive/i);
    expect(alert.textContent).toMatch(/connection reset/i);
    expect(ops.version).toBe(before);
  });

  it('surfaces a loud error when an op fails, without bumping the ops signal', async () => {
    const ops = getMailboxOpsStore();
    const before = ops.version;
    bulkClientMock.mockResolvedValueOnce({
      done: 0,
      total: 1,
      failed: [{ item: { accountId: 'acct-1', uid: 30 }, error: 'imap timeout' }]
    });
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toMatch(/couldn't archive/i);
    expect(alert.textContent).toMatch(/imap timeout/i);
    expect(ops.version).toBe(before);
  });
});
