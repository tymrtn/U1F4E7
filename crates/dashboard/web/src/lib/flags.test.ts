import { describe, expect, it } from 'vitest';
import { hasFlag } from './flags';
import { isSeen } from './reader-api';

describe('flag spelling', () => {
  it('matches the index spelling the server actually returns', () => {
    // List, search and read responses carry "Seen"/"Flagged" (no backslash).
    expect(hasFlag(['Seen', 'Flagged'], '\\Flagged')).toBe(true);
    expect(isSeen(['Seen'])).toBe(true);
  });

  it('matches the wire spelling too, case-insensitively', () => {
    expect(hasFlag(['\\SEEN'], 'seen')).toBe(true);
    expect(isSeen(['\\Seen'])).toBe(true);
  });

  it('does not match a custom keyword that merely contains the name', () => {
    expect(hasFlag(['$NotFlagged'], 'flagged')).toBe(false);
    expect(hasFlag(null, 'seen')).toBe(false);
  });
});
