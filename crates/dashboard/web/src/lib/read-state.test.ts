// Tests for the shared, session-scoped read-state override store.
//
// The reader marks a message \Seen on open; the message list must drop the
// unread-bold styling immediately (incl. after Back with no refetch). Both
// surfaces read/write this one store, so its override precedence is the
// contract they share.
//
// Overrides are keyed by (accountId, normalized folder, uid): IMAP UIDs are
// mailbox-scoped, so the SAME uid in INBOX and Sent for one account must keep
// independent read state — folder is part of the identity, never dropped.

import { describe, expect, it, beforeEach } from 'vitest';
import { readState, __resetReadState } from './read-state.svelte';

beforeEach(() => {
  __resetReadState();
});

describe('readState.isUnread — override precedence', () => {
  it('defers to the backend value when there is no override', () => {
    expect(readState.isUnread('acct-a', 'INBOX', 1, true)).toBe(true);
    expect(readState.isUnread('acct-a', 'INBOX', 2, false)).toBe(false);
  });

  it('renders read (not unread) after markRead, overriding a stale unread backend value', () => {
    readState.markRead('acct-a', 'INBOX', 1);
    expect(readState.isUnread('acct-a', 'INBOX', 1, true)).toBe(false);
  });

  it('renders unread after markUnread, overriding a read backend value', () => {
    readState.markUnread('acct-a', 'INBOX', 1);
    expect(readState.isUnread('acct-a', 'INBOX', 1, false)).toBe(true);
  });

  it('keys overrides by account+uid so accounts do not collide', () => {
    readState.markRead('acct-a', 'INBOX', 1);
    // Same uid+folder, different account: no override, so backend value wins.
    expect(readState.isUnread('acct-b', 'INBOX', 1, true)).toBe(true);
    expect(readState.isUnread('acct-a', 'INBOX', 1, true)).toBe(false);
  });

  it('keys overrides by folder so the same account+uid in two mailboxes stay independent', () => {
    // IMAP UID 1 in INBOX is a different message than UID 1 in Sent. Marking the
    // INBOX one read must NOT bleed into the Sent one.
    readState.markRead('acct-a', 'INBOX', 1);
    expect(readState.isUnread('acct-a', 'INBOX', 1, true)).toBe(false);
    expect(readState.isUnread('acct-a', 'Sent', 1, true)).toBe(true);
    expect(readState.has('acct-a', 'Sent', 1)).toBe(false);

    // And an independent mark on Sent does not disturb INBOX.
    readState.markUnread('acct-a', 'Sent', 1);
    expect(readState.isUnread('acct-a', 'Sent', 1, false)).toBe(true);
    expect(readState.isUnread('acct-a', 'INBOX', 1, true)).toBe(false);
  });

  it('normalizes INBOX case-insensitively so inbox/INBOX are the same mailbox', () => {
    readState.markRead('acct-a', 'inbox', 1);
    expect(readState.isUnread('acct-a', 'INBOX', 1, true)).toBe(false);
  });

  it('reports whether a message has an override via has()', () => {
    expect(readState.has('acct-a', 'INBOX', 1)).toBe(false);
    readState.markRead('acct-a', 'INBOX', 1);
    expect(readState.has('acct-a', 'INBOX', 1)).toBe(true);
  });

  it('__resetReadState clears every override', () => {
    readState.markRead('acct-a', 'INBOX', 1);
    readState.markUnread('acct-a', 'INBOX', 2);
    __resetReadState();
    expect(readState.isUnread('acct-a', 'INBOX', 1, true)).toBe(true);
    expect(readState.isUnread('acct-a', 'INBOX', 2, false)).toBe(false);
  });
});
