// Right-click context menu (#172). The menu is another way into the #170
// action model: same `availableActions()` list, same dispatch, same receipts.
// Opening it never opens the message or marks it read, and the browser's own
// menu stays on text selections, inputs, images and ordinary links.
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
import { EnvelopeApiError } from '$lib/api';
import { SelectionStore } from '$lib/selection.svelte';
import { getMailboxOpsStore, __resetMailboxOpsStore } from '$lib/mailbox-ops.svelte';
import { __resetMessageActions, FOLLOW_UP_SEMANTICS } from '$lib/message-actions.svelte';
import { getContextMenu, __resetContextMenu } from '$lib/context-menu.svelte';
import { __resetReadState } from '$lib/read-state.svelte';

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

function renderRow(over: Record<string, unknown> = {}, selection = new SelectionStore()) {
  const message = mkMessage(over);
  return render(MessageRow, {
    props: { message, selection, orderedKeys: [message.key], verbs: true }
  });
}

function rowEl(key = 'acct-1:30'): HTMLElement {
  return document.querySelector<HTMLElement>(`[data-msg-key="${key}"]`)!;
}

function ctxMenu(): HTMLElement | null {
  return document.querySelector<HTMLElement>('.msg-context-menu');
}

function menuNames(): string[] {
  return Array.from(ctxMenu()!.querySelectorAll('[role="menuitem"]')).map(
    (m) => (m.querySelector('span')?.textContent ?? m.textContent ?? '').trim()
  );
}

async function rightClick(el: Element, x = 120, y = 80) {
  const ev = new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: x, clientY: y, button: 2 });
  el.dispatchEvent(ev);
  await Promise.resolve();
  await Promise.resolve();
  return ev;
}

beforeEach(() => {
  __resetMailboxOpsStore();
  __resetMessageActions();
  __resetContextMenu();
  __resetReadState();
  moveMock.mockResolvedValue({
    ok: true, uid: 30, from_folder: 'INBOX', moved_to: 'Trash', moved_uid: 501, moved_uidvalidity: 9
  });
  flagsMock.mockImplementation(async (_a: string, _u: number, o: { add?: string[]; remove?: string[] }) => ({
    ok: true, uid: 30, added: o.add ?? [], removed: o.remove ?? [], confirmed: true, flags: [],
    seen: (o.add ?? []).includes('\\Seen'), flagged: (o.add ?? []).includes('\\Flagged')
  }));
});

afterEach(() => {
  vi.clearAllMocks();
  window.getSelection()?.removeAllRanges();
});

describe('desktop pointer', () => {
  it('right-click opens the app menu at the pointer without opening the message or marking it read', async () => {
    renderRow();
    const link = rowEl().querySelector('a.msg-body')!;
    const clicks = vi.fn();
    link.addEventListener('click', clicks);
    const ev = await rightClick(link, 140, 90);
    expect(ev.defaultPrevented).toBe(true);
    const menu = ctxMenu()!;
    expect(menu).toBeTruthy();
    expect(menu.getAttribute('role')).toBe('menu');
    expect(menu.style.left).toBe('140px');
    expect(menu.style.top).toBe('90px');
    expect(clicks).not.toHaveBeenCalled();
    expect(flagsMock).not.toHaveBeenCalled();
    expect(moveMock).not.toHaveBeenCalled();
  });

  it('stays inside the viewport when opened near the bottom-right corner', async () => {
    renderRow();
    await rightClick(rowEl().querySelector('a.msg-body')!, window.innerWidth - 2, window.innerHeight - 2);
    const menu = ctxMenu()!;
    vi.spyOn(menu, 'getBoundingClientRect').mockReturnValue({
      width: 200, height: 300, top: 0, left: 0, right: 200, bottom: 300, x: 0, y: 0, toJSON: () => ({})
    } as DOMRect);
    window.dispatchEvent(new Event('resize'));
    await waitFor(() => {
      expect(parseFloat(menu.style.left) + 200).toBeLessThanOrEqual(window.innerWidth);
      expect(parseFloat(menu.style.top) + 300).toBeLessThanOrEqual(window.innerHeight);
    });
  });

  it('right-click on another row retargets: one menu, for the row clicked last', async () => {
    const selection = new SelectionStore();
    render(MessageRow, { props: { message: mkMessage(), selection, orderedKeys: [], verbs: true } });
    render(MessageRow, {
      props: {
        message: mkMessage({ key: 'acct-1:31', uid: 31, subject: 'Second', messageId: '<r31@x>' }),
        selection, orderedKeys: [], verbs: true
      }
    });
    await rightClick(rowEl('acct-1:30').querySelector('a.msg-body')!);
    await rightClick(rowEl('acct-1:31').querySelector('a.msg-body')!);
    expect(document.querySelectorAll('.msg-context-menu')).toHaveLength(1);
    expect(rowEl('acct-1:31').querySelector('.msg-context-menu')).toBeTruthy();
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Flag' }));
    await waitFor(() => expect(flagsMock).toHaveBeenCalledTimes(1));
    expect(flagsMock.mock.calls[0][1]).toBe(31);
    expect(flagsMock.mock.calls[0][2]).toMatchObject({ message_id: '<r31@x>', folder: 'INBOX', uidvalidity: 1700 });
  });

  it('a second right-click on the same row moves the menu instead of closing it', async () => {
    renderRow();
    const link = rowEl().querySelector('a.msg-body')!;
    await rightClick(link, 100, 60);
    await rightClick(link, 200, 70);
    await waitFor(() => expect(ctxMenu()?.style.left).toBe('200px'));
  });

  it('a right-click outside any row closes the menu and keeps the browser menu', async () => {
    renderRow();
    await rightClick(rowEl().querySelector('a.msg-body')!);
    const ev = await rightClick(document.body, 10, 10);
    expect(ev.defaultPrevented).toBe(false);
    await waitFor(() => expect(ctxMenu()).toBeNull());
  });

  it('closes on Escape, outside click, scroll, and route change', async () => {
    renderRow();
    const link = rowEl().querySelector('a.msg-body')!;

    await rightClick(link);
    await fireEvent.keyDown(ctxMenu()!, { key: 'Escape' });
    expect(ctxMenu()).toBeNull();

    await rightClick(link);
    await fireEvent.click(document.body);
    expect(ctxMenu()).toBeNull();

    await rightClick(link);
    window.dispatchEvent(new Event('scroll'));
    await waitFor(() => expect(ctxMenu()).toBeNull());

    // The mail layout closes the menu on navigation through the shared store.
    await rightClick(link);
    getContextMenu().close();
    await waitFor(() => expect(ctxMenu()).toBeNull());
  });

  it('a successful action closes the menu and reports on the row like the primary surface', async () => {
    renderRow({ unread: false });
    await rightClick(rowEl().querySelector('a.msg-body')!);
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Flag' }));
    expect(ctxMenu()).toBeNull();
    await waitFor(() => expect(screen.getByRole('status').textContent).toMatch(/Flagged/));
    expect(screen.getByRole('button', { name: 'Unflag message' })).toBeInTheDocument();
  });

  it('a failed or stale action shows the same row error and refreshes the list', async () => {
    flagsMock.mockRejectedValueOnce(
      new EnvelopeApiError(409, 'stale_uid', 'this message changed on the server', null)
    );
    const ops = getMailboxOpsStore();
    const before = ops.version;
    renderRow();
    await rightClick(rowEl().querySelector('a.msg-body')!);
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Flag' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toMatch(/changed on the server/);
    expect(ops.version).toBe(before + 1);
  });
});

describe('native menu is preserved', () => {
  it('on a text selection inside the row', async () => {
    renderRow();
    const snippet = rowEl().querySelector('.msg-snippet')!;
    const range = document.createRange();
    range.selectNodeContents(snippet);
    window.getSelection()!.addRange(range);
    const ev = await rightClick(snippet);
    expect(ev.defaultPrevented).toBe(false);
    expect(ctxMenu()).toBeNull();
  });

  it('on inputs, images and ordinary links', async () => {
    renderRow();
    const row = rowEl();
    const img = document.createElement('img');
    const input = document.createElement('input');
    const link = document.createElement('a');
    link.href = 'https://example.test/';
    row.querySelector('.msg-line2')!.append(img, input, link);
    for (const el of [img, input, link]) {
      const ev = await rightClick(el);
      expect(ev.defaultPrevented).toBe(false);
    }
    expect(ctxMenu()).toBeNull();
    // The Reply shortcut is an ordinary link too.
    const reply = row.querySelector('a[aria-label="Reply"]')!;
    expect((await rightClick(reply)).defaultPrevented).toBe(false);
  });

  it('on rows with no mailbox actions', async () => {
    const message = mkMessage();
    render(MessageRow, { props: { message, selection: new SelectionStore(), orderedKeys: [], verbs: false } });
    const ev = await rightClick(rowEl().querySelector('a.msg-body')!);
    expect(ev.defaultPrevented).toBe(false);
    expect(ctxMenu()).toBeNull();
  });
});

describe('keyboard', () => {
  it('Shift+F10 and the ContextMenu key open it, arrows move, Enter runs, Escape restores focus', async () => {
    renderRow({ unread: false });
    const link = rowEl().querySelector<HTMLElement>('a.msg-body')!;
    link.focus();
    await fireEvent.keyDown(link, { key: 'F10', shiftKey: true });
    const items = () => Array.from(ctxMenu()!.querySelectorAll<HTMLElement>('[role="menuitem"]'));
    await waitFor(() => expect(document.activeElement).toBe(items()[0]));
    await fireEvent.keyDown(items()[0], { key: 'ArrowDown' });
    expect(document.activeElement).toBe(items()[1]);
    await fireEvent.keyDown(items()[1], { key: 'Escape' });
    expect(ctxMenu()).toBeNull();
    expect(document.activeElement).toBe(link);

    await fireEvent.keyDown(link, { key: 'ContextMenu' });
    await waitFor(() => expect(ctxMenu()).toBeTruthy());
    const flag = items().find((i) => i.textContent?.trim() === 'Flag')!;
    flag.focus();
    await fireEvent.keyDown(flag, { key: 'Enter' });
    await fireEvent.click(flag); // a <button>'s Enter activation
    await waitFor(() => expect(flagsMock).toHaveBeenCalledTimes(1));
  });

  it('the keyboard-fired contextmenu that follows the key does not open a second menu', async () => {
    renderRow();
    const link = rowEl().querySelector<HTMLElement>('a.msg-body')!;
    link.focus();
    await fireEvent.keyDown(link, { key: 'ContextMenu' });
    await rightClick(link, 0, 0);
    expect(document.querySelectorAll('.msg-context-menu')).toHaveLength(1);
  });
});

describe('touch', () => {
  it('long-press opens the menu and the following click does not open the message', async () => {
    vi.useFakeTimers();
    try {
      renderRow();
      const link = rowEl().querySelector('a.msg-body')!;
      const nav = vi.fn((e: Event) => e.defaultPrevented);
      link.addEventListener('click', nav);
      link.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, pointerType: 'touch', clientX: 50, clientY: 40 }));
      await vi.advanceTimersByTimeAsync(600);
      expect(ctxMenu()).toBeTruthy();
      link.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, pointerType: 'touch' }));
      const click = new MouseEvent('click', { bubbles: true, cancelable: true });
      link.dispatchEvent(click);
      expect(click.defaultPrevented).toBe(true);
      expect(flagsMock).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it('a short tap or a drag is not a long-press', async () => {
    vi.useFakeTimers();
    try {
      renderRow();
      const link = rowEl().querySelector('a.msg-body')!;
      link.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, pointerType: 'touch', clientX: 50, clientY: 40 }));
      await vi.advanceTimersByTimeAsync(150);
      link.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, pointerType: 'touch' }));
      await vi.advanceTimersByTimeAsync(600);
      expect(ctxMenu()).toBeNull();

      link.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, pointerType: 'touch', clientX: 50, clientY: 40 }));
      link.dispatchEvent(new PointerEvent('pointermove', { bubbles: true, pointerType: 'touch', clientX: 50, clientY: 90 }));
      await vi.advanceTimersByTimeAsync(600);
      expect(ctxMenu()).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it('the visible More button still gives the same actions without a gesture', async () => {
    renderRow();
    await fireEvent.click(screen.getByRole('button', { name: 'More actions' }));
    expect(screen.getByRole('menuitem', { name: 'Mark read' })).toBeInTheDocument();
  });
});

describe('truthful choices', () => {
  it('offers Open plus the single state-following read toggle, never both labels', async () => {
    renderRow({ unread: true });
    await rightClick(rowEl().querySelector('a.msg-body')!);
    let names = menuNames();
    expect(names[0]).toBe('Open');
    expect(names).toContain('Mark read');
    expect(names).not.toContain('Mark unread');
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Mark read' }));
    await waitFor(() => expect(flagsMock).toHaveBeenCalledTimes(1));
    await rightClick(rowEl().querySelector('a.msg-body')!);
    // The row prop still says unread until the list re-renders; the shared
    // read state already knows better, and the menu follows it.
    names = menuNames();
    expect(names).toContain('Mark unread');
    expect(names).not.toContain('Mark read');
  });

  it('Open is the only item that opens the message', async () => {
    renderRow();
    await rightClick(rowEl().querySelector('a.msg-body')!);
    const open = screen.getByRole('menuitem', { name: 'Open' });
    expect(open.getAttribute('href')).toBe('/mail/unified/acct-1/30?folder=INBOX');
  });

  it('flagged rows offer Unflag; unflagged rows offer Flag', async () => {
    renderRow({ starred: true });
    await rightClick(rowEl().querySelector('a.msg-body')!);
    expect(menuNames()).toContain('Unflag');
    expect(menuNames()).not.toContain('Flag');
  });

  it('inside Junk offers Not junk; inside Trash offers no second delete', async () => {
    const junk = renderRow({ folder: 'Junk' });
    await rightClick(rowEl().querySelector('a.msg-body')!);
    expect(menuNames()).toContain('Not junk: move to Inbox');
    getContextMenu().close();
    junk.unmount();
    renderRow({ folder: 'Trash' });
    await rightClick(rowEl().querySelector('a.msg-body')!);
    expect(menuNames()).not.toContain('Move to Trash');
    expect(menuNames().some((n) => /permanent/i.test(n))).toBe(false);
  });

  it('Delete moves to Trash through the same dispatch, with no permanent delete', async () => {
    renderRow();
    await rightClick(rowEl().querySelector('a.msg-body')!);
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Move to Trash' }));
    await waitFor(() => expect(moveMock).toHaveBeenCalledTimes(1));
    expect(moveMock.mock.calls[0][2]).toMatchObject({ folder: 'INBOX', to_folder: '\\Trash', message_id: '<r30@example.test>' });
  });

  it('Remind and Follow up are listed as unavailable, with the reason', async () => {
    renderRow();
    await rightClick(rowEl().querySelector('a.msg-body')!);
    const remind = screen.getByText('Remind me…').closest('[role="menuitem"]')!;
    expect(remind).toHaveAttribute('aria-disabled', 'true');
    expect(remind).toHaveAccessibleDescription(FOLLOW_UP_SEMANTICS.remind);
  });

  it('a snoozed record offers only Unsnooze (plus the unavailable pair)', async () => {
    renderRow({ folder: undefined, href: undefined, snooze: { id: 'snz-1', returnAt: '2026-10-01T08:00:00Z', status: 'snoozed' } });
    await rightClick(rowEl().querySelector('.msg-body')!);
    const available = Array.from(ctxMenu()!.querySelectorAll('[role="menuitem"]:not([aria-disabled="true"])')).map(
      (m) => m.textContent?.trim()
    );
    expect(available).toEqual(['Unsnooze']);
  });

  it('search rows keep their own account + folder + UID identity', async () => {
    renderRow({ key: 'search:acct-2:77', accountId: 'acct-2', uid: 77, folder: 'Archive', messageId: '<s77@x>' });
    await rightClick(rowEl('search:acct-2:77').querySelector('a.msg-body')!);
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Flag' }));
    await waitFor(() => expect(flagsMock).toHaveBeenCalledTimes(1));
    expect(flagsMock.mock.calls[0].slice(0, 2)).toEqual(['acct-2', 77]);
    expect(flagsMock.mock.calls[0][2]).toMatchObject({ folder: 'Archive', message_id: '<s77@x>' });
  });
});

describe('row vs selection', () => {
  it('when the row is part of a multi-selection, the menu says it acts on this message only and keeps the selection', async () => {
    const selection = new SelectionStore();
    selection.toggle('acct-1:30');
    selection.toggle('acct-1:31');
    renderRow({}, selection);
    await rightClick(rowEl().querySelector('a.msg-body')!);
    expect(ctxMenu()!.textContent).toMatch(/This message only\. 2 selected: use the toolbar to act on all of them\./);
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Flag' }));
    await waitFor(() => expect(flagsMock).toHaveBeenCalledTimes(1));
    expect(selection.count).toBe(2);
  });

  it('with no multi-selection there is no scope note', async () => {
    renderRow();
    await rightClick(rowEl().querySelector('a.msg-body')!);
    expect(ctxMenu()!.textContent).not.toMatch(/This message only/);
  });
});
