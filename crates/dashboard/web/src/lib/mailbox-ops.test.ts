// mailbox-ops — shared signal from any mailbox mutation site (today: the
// reader's archive/delete/star) to the mounted list, which re-fetches. Mirrors
// the read-state / selection / live singleton pattern.
import { describe, expect, it, afterEach } from 'vitest';
import { getMailboxOpsStore, __resetMailboxOpsStore } from '$lib/mailbox-ops.svelte';

afterEach(() => __resetMailboxOpsStore());

describe('mailbox-ops store', () => {
  it('starts at version 0 and increments on each operated() call', () => {
    const ops = getMailboxOpsStore();
    expect(ops.version).toBe(0);
    ops.operated();
    ops.operated();
    expect(ops.version).toBe(2);
  });

  it('is a session singleton', () => {
    getMailboxOpsStore().operated();
    expect(getMailboxOpsStore().version).toBe(1);
  });
});
