// One action model for single-message triage (#170).
//
// Every surface that acts on ONE message — the list row, the row's actions
// menu, the reader, and the right-click menu planned in #172 — describes the
// message as an `ActionTarget` and calls `messageActions.dispatch(target,
// command)`. Nothing else talks to the flag/move/snooze endpoints for a single
// message, so the safety rules live here once:
//
//   • Exact targeting. A target is (account, folder, UID) plus the
//     UIDVALIDITY and Message-ID the list saw. The server refuses (409) when
//     they no longer name the same message; there is no INBOX default.
//   • No duplicate mutation. While a command for a target is in flight, a
//     second dispatch for that target returns `duplicate` without a request.
//   • Honest state. Flag/read state is taken from the server's read-back.
//     A completed write is reported as done even when the list reload that
//     follows it fails; the reload is the list's job, not the write's.
//   • Receipts. Actions that take the message out of view (Junk, Archive,
//     Trash, Snooze) leave a receipt that names where it went and, when the
//     server returned an exact handle, offers the way back.
//
// `availableActions()` is the menu contract: it lists only what can actually
// run for that message, and lists Remind / Follow up as unavailable with the
// reason (there is no backend primitive for either; see FOLLOW_UP_SEMANTICS).

import { api, EnvelopeApiError, type FlagsResult, type MoveResult } from './api';
import { readState } from './read-state.svelte';
import { getMailboxOpsStore } from './mailbox-ops.svelte';
import { looksLikeJunk, looksLikeTrash } from './folder-kinds';
import { formatExactReturn } from './snooze-options';

export interface ActionTarget {
  accountId: string;
  /** The mailbox the UID belongs to. Required: UIDs are folder-scoped. */
  folder: string;
  uid: number;
  uidvalidity?: number | null;
  messageId?: string | null;
  subject?: string | null;
}

export type ActionCommand =
  | { kind: 'flag' }
  | { kind: 'unflag' }
  | { kind: 'mark-read' }
  | { kind: 'mark-unread' }
  | { kind: 'junk' }
  | { kind: 'not-junk' }
  | { kind: 'archive' }
  | { kind: 'trash' }
  | { kind: 'move-back'; toFolder: string }
  | { kind: 'snooze'; returnAt: Date }
  | { kind: 'unsnooze'; snoozeId: string };

export type CommandKind = ActionCommand['kind'];
/** Menu entries include kinds with no backend (always unavailable). */
export type ActionKind = CommandKind | 'remind' | 'follow-up';

export interface Receipt {
  id: number;
  kind: CommandKind;
  text: string;
  target: ActionTarget;
  /** True when the message left the list it was in. */
  removesRow: boolean;
  /** Present only when the server named an exact way back. */
  undo?: { label: string; target: ActionTarget; command: ActionCommand };
}

export type ActionOutcome =
  | { status: 'ok'; receipt: Receipt }
  | { status: 'duplicate' }
  | { status: 'error'; code: string; message: string; stale: boolean };

export type RowActionState =
  | { phase: 'pending'; kind: CommandKind; label: string }
  | { phase: 'done'; receipt: Receipt }
  | { phase: 'error'; kind: CommandKind; code: string; message: string };

export interface ActionDescriptor {
  id: ActionKind;
  label: string;
  available: boolean;
  /** Why it cannot run, shown to the person instead of a fake action. */
  unavailableReason?: string;
}

/**
 * Remind vs Follow up, and why neither is offered as a working action.
 *
 * - A reminder notifies the operator at a time while the message stays where
 *   it is. Envelope has no primitive that notifies without moving the message:
 *   Snooze hides it and returns it, which is a different action and must not be
 *   relabeled as a reminder.
 * - An awaiting-reply follow-up notifies when a reply has NOT arrived by a due
 *   time. The only reply check is the CLI `envelope snooze check`: it runs only
 *   when invoked by hand, it matches any mail from the sender (not replies in
 *   this thread), and nothing in the web app runs it or notifies anyone.
 */
export const FOLLOW_UP_SEMANTICS = {
  remind:
    'Not available: Envelope has no reminder that keeps the message in place and notifies you. Snooze hides the message until a time instead.',
  'follow-up':
    'Not available: nothing in the web app watches for a reply. The CLI’s `envelope snooze check` runs only when you run it and counts any mail from the sender as a reply.'
} as const;

const LABELS: Record<ActionKind, string> = {
  flag: 'Flag',
  unflag: 'Unflag',
  'mark-read': 'Mark read',
  'mark-unread': 'Mark unread',
  junk: 'Move to Junk',
  'not-junk': 'Not junk: move to Inbox',
  archive: 'Archive',
  trash: 'Move to Trash',
  'move-back': 'Move back',
  snooze: 'Snooze…',
  unsnooze: 'Unsnooze',
  remind: 'Remind me…',
  'follow-up': 'Follow up if no reply…'
};

const PENDING: Record<CommandKind, string> = {
  flag: 'Flagging…',
  unflag: 'Unflagging…',
  'mark-read': 'Marking read…',
  'mark-unread': 'Marking unread…',
  junk: 'Moving to Junk…',
  'not-junk': 'Moving to Inbox…',
  archive: 'Archiving…',
  trash: 'Moving to Trash…',
  'move-back': 'Moving back…',
  snooze: 'Snoozing…',
  unsnooze: 'Unsnoozing…'
};

export function actionLabel(kind: ActionKind): string {
  return LABELS[kind];
}

export interface ActionContext {
  folder: string;
  /** null when unknown (e.g. before the reader loads). */
  read: boolean | null;
  flagged: boolean | null;
  /** Set for a snoozed record: the only mailbox handle it has is Unsnooze. */
  snoozeId?: string | null;
}

/** The truthful action list for one message in `ctx`. */
export function availableActions(ctx: ActionContext): ActionDescriptor[] {
  const on = (id: ActionKind): ActionDescriptor => ({ id, label: LABELS[id], available: true });
  const off = (id: 'remind' | 'follow-up'): ActionDescriptor => ({
    id,
    label: LABELS[id],
    available: false,
    unavailableReason: FOLLOW_UP_SEMANTICS[id]
  });
  if (ctx.snoozeId) {
    return [on('unsnooze'), off('remind'), off('follow-up')];
  }
  const out: ActionDescriptor[] = [
    on(ctx.read ? 'mark-unread' : 'mark-read'),
    on(ctx.flagged ? 'unflag' : 'flag'),
    on('snooze'),
    off('remind'),
    off('follow-up'),
    on(looksLikeJunk(ctx.folder) ? 'not-junk' : 'junk'),
    on('archive')
  ];
  if (!looksLikeTrash(ctx.folder)) out.push(on('trash'));
  return out;
}

/** Identity key: account + folder + UID. INBOX folds case (RFC 3501). */
export function targetKey(t: Pick<ActionTarget, 'accountId' | 'folder' | 'uid'>): string {
  const f = (t.folder ?? '').trim();
  const folder = f.toLowerCase() === 'inbox' ? 'INBOX' : f;
  return `${t.accountId}\0${folder}\0${t.uid}`;
}

const STALE_CODES = new Set(['stale_uid', 'message_not_found', 'snoozed_message_not_found']);

function friendlyError(e: unknown): { code: string; message: string; stale: boolean } {
  if (e instanceof EnvelopeApiError) {
    const stale = STALE_CODES.has(e.code);
    return {
      code: e.code,
      stale,
      message: stale
        ? `${e.message}. The list is refreshing.`
        : e.message || 'The server refused the change.'
    };
  }
  const message = e instanceof Error ? e.message : String(e);
  return { code: 'network_error', stale: false, message: message || 'Network error' };
}

/** How long a row-local "done" note stays before the row returns to normal. */
const DONE_NOTE_MS = 4000;

export class MessageActionsStore {
  private rows = $state<Map<string, RowActionState>>(new Map());
  private flagOverrides = $state<Map<string, boolean>>(new Map());
  receipts = $state<Receipt[]>([]);
  private seq = 0;
  private inFlight = new Set<string>();

  rowState(t: Pick<ActionTarget, 'accountId' | 'folder' | 'uid'>): RowActionState | undefined {
    return this.rows.get(targetKey(t));
  }

  /** Reactive: reads only runes state (the pending phase is set in the same
   *  tick as the in-flight guard), so a `$derived` over it re-runs when the
   *  write settles. */
  isBusy(t: Pick<ActionTarget, 'accountId' | 'folder' | 'uid'>): boolean {
    return this.rowState(t)?.phase === 'pending';
  }

  /** Flagged state: the confirmed override when one exists, else the list's. */
  isFlagged(t: Pick<ActionTarget, 'accountId' | 'folder' | 'uid'>, backend: boolean): boolean {
    const o = this.flagOverrides.get(targetKey(t));
    return o === undefined ? backend : o;
  }

  /** Record a flagged state confirmed elsewhere (bulk toolbar). */
  noteFlagged(t: Pick<ActionTarget, 'accountId' | 'folder' | 'uid'>, flagged: boolean) {
    this.flagOverrides = new Map(this.flagOverrides).set(targetKey(t), flagged);
  }

  clearRow(t: Pick<ActionTarget, 'accountId' | 'folder' | 'uid'>) {
    const next = new Map(this.rows);
    next.delete(targetKey(t));
    this.rows = next;
  }

  dismissReceipt(id: number) {
    this.receipts = this.receipts.filter((r) => r.id !== id);
  }

  private setRow(key: string, state: RowActionState | null) {
    const next = new Map(this.rows);
    if (state) next.set(key, state);
    else next.delete(key);
    this.rows = next;
  }

  async dispatch(target: ActionTarget, command: ActionCommand): Promise<ActionOutcome> {
    const key = targetKey(target);
    if (this.inFlight.has(key)) return { status: 'duplicate' };
    this.inFlight.add(key);
    this.setRow(key, { phase: 'pending', kind: command.kind, label: PENDING[command.kind] });

    let receipt: Receipt;
    try {
      receipt = await this.run(target, command);
    } catch (e) {
      const err = friendlyError(e);
      this.inFlight.delete(key);
      this.setRow(key, { phase: 'error', kind: command.kind, code: err.code, message: err.message });
      // A stale handle means the list is out of date: reload it so the
      // eventual state on screen is the server's.
      if (err.stale) getMailboxOpsStore().operated();
      return { status: 'error', ...err };
    }

    this.inFlight.delete(key);
    this.setRow(key, { phase: 'done', receipt });
    if (receipt.removesRow) this.receipts = [...this.receipts, receipt];
    setTimeout(() => {
      const cur = this.rows.get(key);
      if (cur?.phase === 'done' && cur.receipt.id === receipt.id) this.setRow(key, null);
    }, DONE_NOTE_MS);
    // Announce after recording the receipt. A reload that fails afterwards
    // shows as a list error; it can never turn this completed write into a
    // failure.
    try {
      getMailboxOpsStore().operated();
    } catch {
      // Listener errors belong to the list, not to this write.
    }
    return { status: 'ok', receipt };
  }

  private receipt(
    kind: CommandKind,
    target: ActionTarget,
    text: string,
    removesRow: boolean,
    undo?: Receipt['undo']
  ): Receipt {
    return { id: ++this.seq, kind, target, text, removesRow, undo };
  }

  private async run(target: ActionTarget, command: ActionCommand): Promise<Receipt> {
    const ident = {
      folder: target.folder,
      uidvalidity: target.uidvalidity ?? null,
      message_id: target.messageId ?? null
    };
    switch (command.kind) {
      case 'flag':
      case 'unflag': {
        const flag = command.kind === 'flag';
        const res = await api.messageFlags(target.accountId, target.uid, {
          ...ident,
          add: flag ? ['\\Flagged'] : [],
          remove: flag ? [] : ['\\Flagged']
        });
        const flagged = confirmedOr(res, 'flagged', flag);
        this.noteFlagged(target, flagged);
        return this.receipt(command.kind, target, flagged ? 'Flagged' : 'Unflagged', false);
      }
      case 'mark-read':
      case 'mark-unread': {
        const read = command.kind === 'mark-read';
        const res = await api.messageFlags(target.accountId, target.uid, {
          ...ident,
          add: read ? ['\\Seen'] : [],
          remove: read ? [] : ['\\Seen']
        });
        const seen = confirmedOr(res, 'seen', read);
        if (seen) readState.markRead(target.accountId, target.folder, target.uid);
        else readState.markUnread(target.accountId, target.folder, target.uid);
        return this.receipt(command.kind, target, seen ? 'Marked read' : 'Marked unread', false);
      }
      case 'junk':
        return this.move(target, command.kind, '\\Junk', 'Moved to Junk');
      case 'not-junk':
        return this.move(target, command.kind, 'INBOX', 'Moved to Inbox');
      case 'archive':
        return this.move(target, command.kind, '\\Archive', 'Archived');
      case 'trash':
        return this.move(target, command.kind, '\\Trash', 'Moved to Trash');
      case 'move-back':
        return this.move(target, command.kind, command.toFolder, `Moved back to ${command.toFolder}`);
      case 'snooze': {
        const res = await api.snoozeMessage(target.accountId, target.uid, {
          ...ident,
          // A UTC instant: the sweep compares against UTC now.
          return_at: command.returnAt.toISOString(),
          subject: target.subject ?? null
        });
        const back = new Date(res.return_at);
        return this.receipt(
          'snooze',
          target,
          `Snoozed until ${formatExactReturn(back)}`,
          true,
          {
            label: 'Unsnooze',
            target: { accountId: target.accountId, folder: res.snoozed_folder, uid: target.uid },
            command: { kind: 'unsnooze', snoozeId: res.id }
          }
        );
      }
      case 'unsnooze': {
        const res = await api.unsnooze(target.accountId, command.snoozeId);
        return this.receipt('unsnooze', target, `Returned to ${res.moved_to}`, true);
      }
    }
  }

  private async move(
    target: ActionTarget,
    kind: CommandKind,
    toFolder: string,
    verb: string
  ): Promise<Receipt> {
    const res: MoveResult = await api.messageMove(target.accountId, target.uid, {
      folder: target.folder,
      to_folder: toFolder,
      uidvalidity: target.uidvalidity ?? null,
      message_id: target.messageId ?? null
    });
    const text = toFolder.startsWith('\\') ? `${verb} (${res.moved_to})` : verb;
    // Offer the way back only with an exact handle in the destination.
    const undo =
      kind !== 'move-back' && res.moved_uid !== null && res.moved_uid !== undefined
        ? {
            label: 'Move back',
            target: {
              accountId: target.accountId,
              folder: res.moved_to,
              uid: res.moved_uid,
              uidvalidity: res.moved_uidvalidity,
              messageId: target.messageId ?? null,
              subject: target.subject ?? null
            },
            command: { kind: 'move-back' as const, toFolder: res.from_folder ?? target.folder }
          }
        : undefined;
    return this.receipt(kind, target, text, true, undo);
  }
}

function confirmedOr(res: FlagsResult, field: 'seen' | 'flagged', intended: boolean): boolean {
  const v = res?.[field];
  return typeof v === 'boolean' ? v : intended;
}

let singleton: MessageActionsStore | null = null;

export function getMessageActions(): MessageActionsStore {
  if (!singleton) singleton = new MessageActionsStore();
  return singleton;
}

/** Test-only reset. */
export function __resetMessageActions(): void {
  singleton = null;
}
