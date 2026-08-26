// Locating a message inside the unified inbox, and deciding when the cached
// list is lying about being empty.
//
// Both behaviours come from the same incident: a sidecar without credentials
// wrote a per-account `last_error` into the SHARED message index, which the
// index query turns into `message_count = 0` for that account. Every account
// carried one, so the dashboard showed "Inbox is empty" over 703 perfectly
// good indexed rows — and sat that way for a day, because the existing
// stale-refresh predicate only fired on `stale`/`expired`, never `unavailable`.

import { describe, expect, it } from 'vitest';
import {
  folderHints,
  __resetFolderHints
} from './folder-hints.svelte';
import { unifiedNeedsRefresh, positionOf } from './mailbox-position';

function acct(freshness: string, ok = true) {
  return { account_id: 'a1', freshness, ok };
}

describe('unifiedNeedsRefresh', () => {
  it('refreshes when an account cache is stale or expired', () => {
    expect(unifiedNeedsRefresh({ freshness: 'stale', accounts: [], messages: [] })).toBe(true);
    expect(unifiedNeedsRefresh({ freshness: 'expired', accounts: [], messages: [] })).toBe(true);
    expect(
      unifiedNeedsRefresh({ freshness: 'fresh', accounts: [acct('stale')], messages: [{}] })
    ).toBe(true);
  });

  /// THE regression: every account unavailable and not one message rendered.
  /// `unavailable` was deliberately excluded from the stale check so two
  /// permanently-broken accounts could not re-IMAP the fleet on every open —
  /// but that also meant a fully-blanked inbox could never heal itself.
  it('refreshes when the list is empty but accounts exist', () => {
    const blanked = {
      freshness: 'partial',
      accounts: [acct('unavailable', false), acct('unavailable', false)],
      messages: []
    };
    expect(unifiedNeedsRefresh(blanked)).toBe(true);
  });

  /// The steady state that must NOT re-IMAP: a couple of accounts are
  /// permanently unreachable, but mail is rendering fine.
  it('does not refresh when messages rendered, even with unreachable accounts', () => {
    const steady = {
      freshness: 'partial',
      accounts: [acct('fresh'), acct('unavailable', false), acct('unavailable', false)],
      messages: [{}, {}, {}]
    };
    expect(unifiedNeedsRefresh(steady)).toBe(false);
  });

  /// A genuinely empty mailbox must not spin: no accounts, nothing to refresh.
  it('does not refresh an empty list with no accounts', () => {
    expect(unifiedNeedsRefresh({ freshness: 'fresh', accounts: [], messages: [] })).toBe(false);
  });

  /// Every account reporting a healthy empty mailbox is genuinely empty.
  it('does not refresh when accounts are fresh and simply have no mail', () => {
    expect(
      unifiedNeedsRefresh({
        freshness: 'fresh',
        accounts: [acct('empty'), acct('empty')],
        messages: []
      })
    ).toBe(false);
  });
});

describe('positionOf', () => {
  const list = [
    { account_id: 'a1', uid: 10 },
    { account_id: 'a1', uid: 11 },
    { account_id: 'a2', uid: 10 }
  ];

  it('reports a 1-based position and total', () => {
    expect(positionOf(list, 'a1', 11)).toEqual({ index: 1, position: 2, total: 3 });
  });

  /// UIDs are mailbox-scoped, so uid alone is not an identity — a2:10 must not
  /// match a1:10.
  it('matches on account AND uid together', () => {
    expect(positionOf(list, 'a2', 10)).toEqual({ index: 2, position: 3, total: 3 });
  });

  it('returns null when the message is not in the loaded page', () => {
    expect(positionOf(list, 'a1', 999)).toBeNull();
    expect(positionOf(list, null, 10)).toBeNull();
    expect(positionOf(list, 'a1', null)).toBeNull();
  });
});

describe('folderHints', () => {
  it('resolves a folder for a message the list has seen', () => {
    __resetFolderHints();
    folderHints.remember([
      { account_id: 'a1', uid: 10, folder: '[Gmail]/All Mail' },
      { account_id: 'a1', uid: 11, folder: 'INBOX' }
    ]);
    expect(folderHints.folderFor('a1', 10)).toBe('[Gmail]/All Mail');
    expect(folderHints.folderFor('a1', 11)).toBe('INBOX');
  });

  it('is account-scoped and returns null when unseen', () => {
    __resetFolderHints();
    folderHints.remember([{ account_id: 'a1', uid: 10, folder: 'Archive' }]);
    expect(folderHints.folderFor('a2', 10)).toBeNull();
    expect(folderHints.folderFor('a1', 99)).toBeNull();
  });
});
