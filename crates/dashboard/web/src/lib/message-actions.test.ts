// #170 action model: exact targeting, duplicate prevention, honest state,
// receipts with a real way back, and truthful Remind/Follow-up availability.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { flagsMock, moveMock, snoozeMock, unsnoozeMock } = vi.hoisted(() => ({
  flagsMock: vi.fn(),
  moveMock: vi.fn(),
  snoozeMock: vi.fn(),
  unsnoozeMock: vi.fn()
}));

vi.mock('./api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./api')>();
  return {
    ...actual,
    api: {
      ...actual.api,
      messageFlags: flagsMock,
      messageMove: moveMock,
      snoozeMessage: snoozeMock,
      unsnooze: unsnoozeMock
    }
  };
});

import { EnvelopeApiError } from './api';
import {
  availableActions,
  FOLLOW_UP_SEMANTICS,
  getMessageActions,
  targetKey,
  __resetMessageActions,
  type ActionTarget
} from './message-actions.svelte';
import { readState, __resetReadState } from './read-state.svelte';
import { getMailboxOpsStore, __resetMailboxOpsStore } from './mailbox-ops.svelte';

const T: ActionTarget = {
  accountId: 'acct-1',
  folder: 'Receipts',
  uid: 41,
  uidvalidity: 7001,
  messageId: '<m41@example.test>',
  subject: 'Invoice'
};

function flagsOk(over: Record<string, unknown> = {}) {
  return {
    ok: true,
    uid: 41,
    added: [],
    removed: [],
    confirmed: true,
    flags: [],
    seen: false,
    flagged: false,
    ...over
  };
}

beforeEach(() => {
  __resetMessageActions();
  __resetMailboxOpsStore();
  __resetReadState();
});

afterEach(() => {
  vi.clearAllMocks();
  vi.useRealTimers();
});

describe('exact targeting', () => {
  it('flag sends the row’s own folder, UIDVALIDITY and Message-ID (no INBOX default)', async () => {
    flagsMock.mockResolvedValue(flagsOk({ flagged: true, flags: ['Flagged'] }));
    const out = await getMessageActions().dispatch(T, { kind: 'flag' });
    expect(out.status).toBe('ok');
    expect(flagsMock).toHaveBeenCalledWith('acct-1', 41, {
      folder: 'Receipts',
      uidvalidity: 7001,
      message_id: '<m41@example.test>',
      add: ['\\Flagged'],
      remove: []
    });
  });

  it('keys identity by account + folder + uid: the same UID in another folder is another message', () => {
    expect(targetKey(T)).not.toBe(targetKey({ ...T, folder: 'INBOX' }));
    expect(targetKey({ ...T, folder: 'inbox' })).toBe(targetKey({ ...T, folder: 'INBOX' }));
  });
});

describe('duplicate mutation and progress', () => {
  it('a repeated click while the first is in flight makes no second request', async () => {
    let release!: (v: unknown) => void;
    moveMock.mockReturnValue(new Promise((r) => (release = r)));
    const store = getMessageActions();
    const first = store.dispatch(T, { kind: 'junk' });
    expect(store.isBusy(T)).toBe(true);
    expect(store.rowState(T)).toMatchObject({ phase: 'pending', label: 'Moving to Junk…' });
    const second = await store.dispatch(T, { kind: 'junk' });
    expect(second.status).toBe('duplicate');
    release({
      ok: true,
      uid: 41,
      from_folder: 'Receipts',
      moved_to: 'Junk',
      moved_uid: 9,
      moved_uidvalidity: 5
    });
    expect((await first).status).toBe('ok');
    expect(moveMock).toHaveBeenCalledTimes(1);
    expect(store.isBusy(T)).toBe(false);
  });

  it('a different message stays actionable while another is in flight', async () => {
    moveMock.mockReturnValue(new Promise(() => {}));
    flagsMock.mockResolvedValue(flagsOk({ flagged: true }));
    const store = getMessageActions();
    void store.dispatch(T, { kind: 'archive' });
    const other = await store.dispatch({ ...T, uid: 42 }, { kind: 'flag' });
    expect(other.status).toBe('ok');
  });
});

describe('honest state', () => {
  it('uses the server read-back, not the requested state', async () => {
    // The STORE ran, but another client unflagged it before the read-back.
    flagsMock.mockResolvedValue(flagsOk({ flagged: false }));
    const store = getMessageActions();
    await store.dispatch(T, { kind: 'flag' });
    expect(store.isFlagged(T, true)).toBe(false);
  });

  it('mark unread / mark read reconcile the shared read state every surface renders', async () => {
    flagsMock.mockResolvedValueOnce(flagsOk({ seen: false }));
    const store = getMessageActions();
    await store.dispatch(T, { kind: 'mark-unread' });
    expect(flagsMock.mock.calls[0][2]).toMatchObject({ add: [], remove: ['\\Seen'] });
    expect(readState.isUnread('acct-1', 'Receipts', 41, false)).toBe(true);

    flagsMock.mockResolvedValueOnce(flagsOk({ seen: true }));
    await store.dispatch(T, { kind: 'mark-read' });
    expect(flagsMock.mock.calls[1][2]).toMatchObject({ add: ['\\Seen'], remove: [] });
    expect(readState.isUnread('acct-1', 'Receipts', 41, true)).toBe(false);
  });

  it('an unconfirmed read-back falls back to the state that was written', async () => {
    flagsMock.mockResolvedValue(flagsOk({ confirmed: false, flags: null, flagged: null }));
    const store = getMessageActions();
    await store.dispatch(T, { kind: 'flag' });
    expect(store.isFlagged(T, false)).toBe(true);
  });

  it('a stale handle is an error, not a success, and reloads the list', async () => {
    flagsMock.mockRejectedValue(
      new EnvelopeApiError(409, 'stale_uid', 'the mailbox was reset', {})
    );
    const ops = getMailboxOpsStore();
    const before = ops.version;
    const store = getMessageActions();
    const out = await store.dispatch(T, { kind: 'flag' });
    expect(out).toMatchObject({ status: 'error', code: 'stale_uid', stale: true });
    expect(store.rowState(T)).toMatchObject({ phase: 'error', code: 'stale_uid' });
    expect(store.isFlagged(T, false)).toBe(false);
    expect(ops.version).toBe(before + 1);
  });

  it('a failed operation leaves the row retryable (not stuck busy)', async () => {
    moveMock.mockRejectedValueOnce(new Error('socket closed'));
    const store = getMessageActions();
    const out = await store.dispatch(T, { kind: 'junk' });
    expect(out).toMatchObject({ status: 'error', code: 'network_error', stale: false });
    expect(store.isBusy(T)).toBe(false);
  });

  it('a successful write followed by a failed reload is still reported as done', async () => {
    flagsMock.mockResolvedValue(flagsOk({ flagged: true }));
    const ops = getMailboxOpsStore();
    vi.spyOn(ops, 'operated').mockImplementation(() => {
      throw new Error('reload failed');
    });
    const store = getMessageActions();
    const out = await store.dispatch(T, { kind: 'flag' });
    expect(out.status).toBe('ok');
    expect(store.rowState(T)).toMatchObject({ phase: 'done' });
  });

  it('the row-local done note clears itself', async () => {
    vi.useFakeTimers();
    flagsMock.mockResolvedValue(flagsOk({ flagged: true }));
    const store = getMessageActions();
    await store.dispatch(T, { kind: 'flag' });
    expect(store.rowState(T)?.phase).toBe('done');
    vi.advanceTimersByTime(4001);
    expect(store.rowState(T)).toBeUndefined();
  });
});

describe('junk and recovery', () => {
  it('junk moves to the provider-resolved \\Junk and offers an exact Move back', async () => {
    moveMock.mockResolvedValueOnce({
      ok: true,
      uid: 41,
      from_folder: 'Receipts',
      moved_to: '[Gmail]/Spam',
      moved_uid: 903,
      moved_uidvalidity: 12
    });
    const store = getMessageActions();
    const out = await store.dispatch(T, { kind: 'junk' });
    expect(moveMock.mock.calls[0][2]).toMatchObject({ folder: 'Receipts', to_folder: '\\Junk' });
    if (out.status !== 'ok') throw new Error('expected ok');
    expect(out.receipt.text).toBe('Moved to Junk ([Gmail]/Spam)');
    expect(out.receipt.removesRow).toBe(true);
    expect(store.receipts).toHaveLength(1);
    expect(out.receipt.undo).toMatchObject({
      label: 'Move back',
      target: { folder: '[Gmail]/Spam', uid: 903, uidvalidity: 12 },
      command: { kind: 'move-back', toFolder: 'Receipts' }
    });

    moveMock.mockResolvedValueOnce({
      ok: true,
      uid: 903,
      from_folder: '[Gmail]/Spam',
      moved_to: 'Receipts',
      moved_uid: 44,
      moved_uidvalidity: 7001
    });
    const back = await store.dispatch(out.receipt.undo!.target, out.receipt.undo!.command);
    expect(back.status).toBe('ok');
    expect(moveMock.mock.calls[1]).toEqual([
      'acct-1',
      903,
      {
        folder: '[Gmail]/Spam',
        to_folder: 'Receipts',
        uidvalidity: 12,
        message_id: '<m41@example.test>'
      }
    ]);
  });

  it('without an exact destination handle, no Move back is offered', async () => {
    moveMock.mockResolvedValue({
      ok: true,
      uid: 41,
      from_folder: 'Receipts',
      moved_to: 'Junk',
      moved_uid: null,
      moved_uidvalidity: null
    });
    const out = await getMessageActions().dispatch(T, { kind: 'junk' });
    if (out.status !== 'ok') throw new Error('expected ok');
    expect(out.receipt.undo).toBeUndefined();
  });

  it('inside Junk the action is Not junk, which moves to Inbox', async () => {
    moveMock.mockResolvedValue({
      ok: true,
      uid: 5,
      from_folder: 'Junk',
      moved_to: 'INBOX',
      moved_uid: 77,
      moved_uidvalidity: 1
    });
    await getMessageActions().dispatch({ ...T, folder: 'Junk', uid: 5 }, { kind: 'not-junk' });
    expect(moveMock.mock.calls[0][2]).toMatchObject({ folder: 'Junk', to_folder: 'INBOX' });
  });
});

describe('snooze and unsnooze', () => {
  it('snooze sends a UTC instant and the receipt names the exact return and Unsnooze', async () => {
    const at = new Date('2026-11-02T13:00:00Z');
    snoozeMock.mockResolvedValue({
      ok: true,
      id: 'snz-1',
      uid: 41,
      original_folder: 'Receipts',
      return_at: '2026-11-02T13:00:00Z',
      snoozed_folder: 'Snoozed',
      message_id: 'm41@example.test'
    });
    unsnoozeMock.mockResolvedValue({ ok: true, id: 'snz-1', moved_to: 'Receipts', record_cleared: true });
    const store = getMessageActions();
    const out = await store.dispatch(T, { kind: 'snooze', returnAt: at });
    expect(snoozeMock.mock.calls[0][2]).toMatchObject({
      folder: 'Receipts',
      return_at: '2026-11-02T13:00:00.000Z',
      uidvalidity: 7001,
      message_id: '<m41@example.test>'
    });
    if (out.status !== 'ok') throw new Error('expected ok');
    expect(out.receipt.text).toMatch(/^Snoozed until /);
    expect(out.receipt.undo?.command).toEqual({ kind: 'unsnooze', snoozeId: 'snz-1' });

    const back = await store.dispatch(out.receipt.undo!.target, out.receipt.undo!.command);
    expect(back.status).toBe('ok');
    expect(unsnoozeMock).toHaveBeenCalledWith('acct-1', 'snz-1');
  });
});

describe('availableActions (the menu contract #172 reuses)', () => {
  const ids = (xs: ReturnType<typeof availableActions>) => xs.map((x) => x.id);

  it('offers the inverse of the current read and flag state', () => {
    expect(ids(availableActions({ folder: 'INBOX', read: true, flagged: true }))).toEqual(
      expect.arrayContaining(['mark-unread', 'unflag'])
    );
    expect(ids(availableActions({ folder: 'INBOX', read: false, flagged: false }))).toEqual(
      expect.arrayContaining(['mark-read', 'flag'])
    );
  });

  it('lists Remind and Follow up as unavailable, with the reason, never as wired actions', () => {
    const list = availableActions({ folder: 'INBOX', read: true, flagged: false });
    const remind = list.find((a) => a.id === 'remind')!;
    const follow = list.find((a) => a.id === 'follow-up')!;
    expect(remind.available).toBe(false);
    expect(follow.available).toBe(false);
    expect(remind.unavailableReason).toBe(FOLLOW_UP_SEMANTICS.remind);
    expect(follow.unavailableReason).toBe(FOLLOW_UP_SEMANTICS['follow-up']);
    expect(remind.label).not.toMatch(/snooze/i);
  });

  it('in Junk offers Not junk; in Trash drops Move to Trash', () => {
    expect(ids(availableActions({ folder: '[Gmail]/Spam', read: true, flagged: false }))).toContain(
      'not-junk'
    );
    expect(ids(availableActions({ folder: 'Junk E-mail', read: true, flagged: false }))).not.toContain(
      'junk'
    );
    expect(ids(availableActions({ folder: 'Deleted Items', read: true, flagged: false }))).not.toContain(
      'trash'
    );
  });

  it('a snoozed record offers only Unsnooze as a mailbox action', () => {
    const list = availableActions({ folder: 'Snoozed', read: null, flagged: null, snoozeId: 's1' });
    expect(list.filter((a) => a.available).map((a) => a.id)).toEqual(['unsnooze']);
  });
});
