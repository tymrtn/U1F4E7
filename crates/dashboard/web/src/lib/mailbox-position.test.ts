// Locating a message inside the unified inbox. (When to sync the list is
// mailbox-sync.svelte.ts; a failed sync no longer hides indexed rows, #171.)

import { describe, expect, it } from 'vitest';
import {
  folderHints,
  __resetFolderHints
} from './folder-hints.svelte';
import { positionOf } from './mailbox-position';

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
