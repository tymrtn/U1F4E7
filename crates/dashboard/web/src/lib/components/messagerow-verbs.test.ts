// MessageRow actions (#170). The row dispatches single-message commands
// through the shared action model with its exact identity, shows progress,
// errors, and done notes on the row, exposes every action through a visible
// More actions menu (no hover or selection needed), and never opens a send
// path. Delegate is present but disabled until its backend lands.
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { moveMock, flagsMock, snoozeMock, unsnoozeMock } = vi.hoisted(() => ({
  moveMock: vi.fn(),
  flagsMock: vi.fn(),
  snoozeMock: vi.fn(),
  unsnoozeMock: vi.fn()
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return {
    ...actual,
    api: {
      ...actual.api,
      messageMove: moveMock,
      messageFlags: flagsMock,
      snoozeMessage: snoozeMock,
      unsnooze: unsnoozeMock
    }
  };
});

import MessageRow from './MessageRow.svelte';
import { SelectionStore } from '$lib/selection.svelte';
import { getMailboxOpsStore, __resetMailboxOpsStore } from '$lib/mailbox-ops.svelte';
import { __resetMessageActions, FOLLOW_UP_SEMANTICS } from '$lib/message-actions.svelte';

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
    uidvalidity: 1700,
    messageId: '<r30@example.test>',
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

const MOVED = {
  ok: true,
  uid: 30,
  from_folder: 'INBOX',
  moved_to: 'Archive',
  moved_uid: 501,
  moved_uidvalidity: 9
};

beforeEach(() => {
  __resetMailboxOpsStore();
  __resetMessageActions();
  moveMock.mockResolvedValue(MOVED);
  flagsMock.mockResolvedValue({
    ok: true,
    uid: 30,
    added: ['\\Flagged'],
    removed: [],
    confirmed: true,
    flags: ['Flagged'],
    seen: false,
    flagged: true
  });
  snoozeMock.mockResolvedValue({
    ok: true,
    id: 'snz-1',
    uid: 30,
    original_folder: 'INBOX',
    return_at: '2030-01-07T08:00:00Z',
    snoozed_folder: 'Snoozed',
    message_id: 'r30@example.test'
  });
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

describe('MessageRow — actions', () => {
  it('omits mailbox actions when the row has no folder', () => {
    renderRow({ folder: undefined });
    expect(screen.queryByRole('button', { name: 'Archive' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'More actions' })).toBeNull();
  });

  it('archive dispatches a \\Archive move with the row’s exact identity and bumps the ops signal', async () => {
    const ops = getMailboxOpsStore();
    const before = ops.version;
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    await waitFor(() => expect(moveMock).toHaveBeenCalledTimes(1));
    expect(moveMock.mock.calls[0]).toEqual([
      'acct-1',
      30,
      { folder: 'INBOX', to_folder: '\\Archive', uidvalidity: 1700, message_id: '<r30@example.test>' }
    ]);
    await waitFor(() => expect(ops.version).toBe(before + 1));
  });

  it('delete moves to \\Trash (reversible)', async () => {
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(moveMock).toHaveBeenCalledTimes(1));
    expect(moveMock.mock.calls[0][2]).toMatchObject({ folder: 'INBOX', to_folder: '\\Trash' });
  });

  it('a row in another folder acts on that folder, never INBOX', async () => {
    renderRow({ folder: 'Clients/Acme', key: 'acct-1:Clients/Acme:30' });
    await fireEvent.click(screen.getByRole('button', { name: 'Move to Junk' }));
    await waitFor(() => expect(moveMock).toHaveBeenCalledTimes(1));
    expect(moveMock.mock.calls[0][2]).toMatchObject({ folder: 'Clients/Acme', to_folder: '\\Junk' });
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

  it('flag is always visible (not hover-only) and sets \\Flagged on this exact message', async () => {
    renderRow();
    const flag = screen.getByRole('button', { name: 'Flag message' });
    expect(flag.closest('.msg-verbs')).toBeNull();
    expect(flag).toHaveAttribute('aria-pressed', 'false');
    await fireEvent.click(flag);
    await waitFor(() => expect(flagsMock).toHaveBeenCalledTimes(1));
    expect(flagsMock.mock.calls[0][2]).toMatchObject({
      folder: 'INBOX',
      add: ['\\Flagged'],
      uidvalidity: 1700,
      message_id: '<r30@example.test>'
    });
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Unflag message' })).toHaveAttribute('aria-pressed', 'true')
    );
    expect(await screen.findByText('Flagged')).toBeInTheDocument();
  });

  it('a repeated click while the write is in flight sends one request and shows progress', async () => {
    let release!: (v: unknown) => void;
    moveMock.mockReturnValueOnce(new Promise((r) => (release = r)));
    renderRow();
    const archive = screen.getByRole('button', { name: 'Archive' });
    await fireEvent.click(archive);
    expect(await screen.findByRole('status')).toHaveTextContent('Archiving…');
    expect(archive).toBeDisabled();
    await fireEvent.click(archive);
    await fireEvent.keyDown(screen.getByRole('row'), { key: 'e' });
    expect(moveMock).toHaveBeenCalledTimes(1);
    release(MOVED);
    await waitFor(() => expect(archive).not.toBeDisabled());
  });

  it('surfaces a loud row-local error when a move fails, without bumping the ops signal', async () => {
    const { EnvelopeApiError } = await import('$lib/api');
    moveMock.mockRejectedValueOnce(new EnvelopeApiError(502, 'http_502', 'connection reset', null));
    const ops = getMailboxOpsStore();
    const before = ops.version;
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'Archive' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toMatch(/couldn't archive/i);
    expect(alert.textContent).toMatch(/connection reset/i);
    expect(ops.version).toBe(before);
    await fireEvent.click(screen.getByRole('button', { name: 'Dismiss error' }));
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('a stale handle says so and refreshes the list instead of claiming success', async () => {
    const { EnvelopeApiError } = await import('$lib/api');
    flagsMock.mockRejectedValueOnce(
      new EnvelopeApiError(409, 'message_not_found', 'this message is no longer in that folder', null)
    );
    const ops = getMailboxOpsStore();
    const before = ops.version;
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'Flag message' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toMatch(/no longer in that folder/);
    expect(ops.version).toBe(before + 1);
    expect(screen.getByRole('button', { name: 'Flag message' })).toHaveAttribute('aria-pressed', 'false');
  });

  it('Shift+U toggles read state from the keyboard', async () => {
    renderRow({ unread: true });
    await fireEvent.keyDown(screen.getByRole('row'), { key: 'U' });
    await waitFor(() => expect(flagsMock).toHaveBeenCalledTimes(1));
    expect(flagsMock.mock.calls[0][2]).toMatchObject({ add: ['\\Seen'], remove: [] });
  });
});

describe('MessageRow — More actions menu', () => {
  it('is reachable without hover or selection and lists the truthful actions', async () => {
    renderRow({ unread: true, starred: false });
    const more = screen.getByRole('button', { name: 'More actions' });
    expect(more.closest('.msg-verbs')).toBeNull();
    expect(more).toHaveAttribute('aria-haspopup', 'menu');
    await fireEvent.click(more);
    const names = screen.getAllByRole('menuitem').map((m) => m.textContent?.trim() ?? '');
    expect(names[0]).toBe('Mark read');
    expect(names).toEqual(
      expect.arrayContaining(['Flag', 'Snooze…', 'Move to Junk', 'Archive', 'Move to Trash'])
    );
  });

  it('shows one read toggle whose label follows the state (Tyler 2026-09-29)', async () => {
    renderRow({ unread: false });
    await fireEvent.click(screen.getByRole('button', { name: 'More actions' }));
    const names = screen.getAllByRole('menuitem').map((m) => m.textContent?.trim() ?? '');
    expect(names).toContain('Mark unread');
    expect(names).not.toContain('Mark read');
  });

  it('shows Remind and Follow up as unavailable with the reason, and does nothing when chosen', async () => {
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'More actions' }));
    const remind = screen.getByText('Remind me…').closest('[role="menuitem"]')!;
    const follow = screen.getByText('Follow up if no reply…').closest('[role="menuitem"]')!;
    expect(remind).toHaveAttribute('aria-disabled', 'true');
    expect(follow).toHaveAttribute('aria-disabled', 'true');
    expect(remind).toHaveAccessibleDescription(FOLLOW_UP_SEMANTICS.remind);
    expect(follow).toHaveAccessibleDescription(FOLLOW_UP_SEMANTICS['follow-up']);
    await fireEvent.click(remind);
    await fireEvent.click(follow);
    expect(flagsMock).not.toHaveBeenCalled();
    expect(moveMock).not.toHaveBeenCalled();
    expect(snoozeMock).not.toHaveBeenCalled();
  });

  it('keyboard: ArrowDown opens and focuses the first item, arrows move, Escape returns focus', async () => {
    renderRow();
    const more = screen.getByRole('button', { name: 'More actions' });
    more.focus();
    await fireEvent.keyDown(more, { key: 'ArrowDown' });
    const items = await screen.findAllByRole('menuitem');
    await waitFor(() => expect(document.activeElement).toBe(items[0]));
    await fireEvent.keyDown(items[0], { key: 'ArrowDown' });
    expect(document.activeElement).toBe(items[1]);
    await fireEvent.keyDown(items[1], { key: 'ArrowUp' });
    expect(document.activeElement).toBe(items[0]);
    await fireEvent.keyDown(items[0], { key: 'Escape' });
    expect(screen.queryByRole('menu')).toBeNull();
    expect(document.activeElement).toBe(more);
  });

  it('snooze lists explicit exact times and dispatches the chosen one as a UTC instant', async () => {
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'More actions' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Snooze…' }));
    const menu = await screen.findByRole('menu', { name: 'Snooze until' });
    const next = menu.querySelector('[data-snooze="next-week"]') as HTMLElement;
    expect(next.textContent).toMatch(/Next week/);
    await fireEvent.click(next);
    await waitFor(() => expect(snoozeMock).toHaveBeenCalledTimes(1));
    const [accountId, uid, opts] = snoozeMock.mock.calls[0];
    expect(accountId).toBe('acct-1');
    expect(uid).toBe(30);
    expect(opts.folder).toBe('INBOX');
    expect(opts.return_at).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/);
  });

  it('a custom snooze time in the past is refused before any request', async () => {
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'More actions' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Snooze…' }));
    const input = screen.getByLabelText('Pick a date and time') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: '2001-01-01T09:00' } });
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Snooze until then' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Pick a time in the future.');
    expect(snoozeMock).not.toHaveBeenCalled();
  });

  it('snooze failure shows a row-local error without bumping the ops signal', async () => {
    const { EnvelopeApiError } = await import('$lib/api');
    snoozeMock.mockRejectedValueOnce(new EnvelopeApiError(502, 'imap_error', 'snooze store failed', null));
    const ops = getMailboxOpsStore();
    const before = ops.version;
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'More actions' }));
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Snooze…' }));
    await fireEvent.click(screen.getAllByRole('menuitem')[0]);
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toMatch(/couldn't snooze/i);
    expect(ops.version).toBe(before);
  });
});

describe('MessageRow — snoozed record', () => {
  const snoozedRow = {
    key: 'snoozed:snz-9',
    folder: 'Snoozed',
    href: undefined,
    snooze: { id: 'snz-9', returnAt: '2030-01-07T13:00:00Z', status: 'snoozed' }
  };

  it('shows the exact return time and offers only Unsnooze', async () => {
    unsnoozeMock.mockResolvedValue({ ok: true, id: 'snz-9', moved_to: 'INBOX', record_cleared: true });
    const { container } = renderRow(snoozedRow, false);
    expect(container.querySelector('.msg-snooze')?.textContent).toMatch(/^\s*Returns /);
    // Not a link: the stored UID is not a handle inside the Snoozed folder.
    expect(container.querySelector('a.msg-body')).toBeNull();
    await fireEvent.click(screen.getByRole('button', { name: 'More actions' }));
    const enabled = screen
      .getAllByRole('menuitem')
      .filter((m) => m.getAttribute('aria-disabled') !== 'true')
      .map((m) => m.textContent?.trim());
    expect(enabled).toEqual(['Unsnooze']);
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Unsnooze' }));
    await waitFor(() => expect(unsnoozeMock).toHaveBeenCalledWith('acct-1', 'snz-9'));
  });

  it('marks an overdue snooze as overdue', () => {
    const { container } = renderRow(
      { ...snoozedRow, snooze: { ...snoozedRow.snooze, status: 'overdue' } },
      false
    );
    expect(container.querySelector('.msg-snooze.is-overdue')?.textContent).toMatch(/Overdue/);
  });
});
