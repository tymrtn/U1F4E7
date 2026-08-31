// Digest model: thread grouping must be order-preserving, dedupe by thread
// id, and never invent data — a message without a thread id is its own row.
import { describe, expect, it } from 'vitest';
import { DIGEST_SECTIONS, groupIntoThreads } from './digest';
import type { UnifiedInboxMessage } from './api';

function msg(over: Partial<UnifiedInboxMessage>): UnifiedInboxMessage {
  return {
    uid: 1,
    subject: 'subject',
    from_addr: 'sender@example.com',
    date: '2026-08-28T09:00:00Z',
    message_id: '<m@x>',
    unread: false,
    account_id: 'acc-1',
    account_username: 'acc@example.com',
    account_display_name: null,
    folder: 'INBOX',
    uidvalidity: 1,
    snippet: null,
    thread_id: null,
    indexed_at: null,
    index_freshness: 'fresh',
    ...over
  } as UnifiedInboxMessage;
}

describe('groupIntoThreads', () => {
  it('collapses messages sharing a thread id into one row anchored at the newest', () => {
    const rows = groupIntoThreads([
      msg({ uid: 30, thread_id: 't1', subject: 'newest', unread: true }),
      msg({ uid: 20, thread_id: 't1', subject: 'older' }),
      msg({ uid: 10, thread_id: 't2', subject: 'other thread' })
    ]);
    expect(rows).toHaveLength(2);
    expect(rows[0].subject).toBe('newest');
    expect(rows[0].count).toBe(2);
    expect(rows[0].uid).toBe(30);
    expect(rows[1].subject).toBe('other thread');
  });

  it('keeps a thread unread when any loaded message is unread', () => {
    const rows = groupIntoThreads([
      msg({ uid: 2, thread_id: 't1', unread: false }),
      msg({ uid: 1, thread_id: 't1', unread: true })
    ]);
    expect(rows[0].unread).toBe(true);
  });

  it('gives thread-less messages their own account-scoped row key', () => {
    const rows = groupIntoThreads([
      msg({ uid: 5, account_id: 'acc-1' }),
      msg({ uid: 5, account_id: 'acc-2' })
    ]);
    expect(rows).toHaveLength(2);
    expect(new Set(rows.map((r) => r.key)).size).toBe(2);
  });

  it('preserves newest-first input order across threads', () => {
    const rows = groupIntoThreads([
      msg({ uid: 9, thread_id: 'b', subject: 'B' }),
      msg({ uid: 8, thread_id: 'a', subject: 'A' }),
      msg({ uid: 7, thread_id: 'b' })
    ]);
    expect(rows.map((r) => r.subject)).toEqual(['B', 'A']);
  });
});

describe('DIGEST_SECTIONS', () => {
  it('ships every category honest: nothing claims to be wired yet', () => {
    // Flipping a section to wired requires an actual categorize backend —
    // this assertion is the tripwire against faking it in the UI.
    expect(DIGEST_SECTIONS.every((s) => !s.wired)).toBe(true);
  });

  it('keeps attention order: do → wait → noise', () => {
    const tones = DIGEST_SECTIONS.map((s) => s.tone);
    const firstWait = tones.indexOf('wait');
    const firstNoise = tones.indexOf('noise');
    expect(tones.lastIndexOf('do')).toBeLessThan(firstWait);
    expect(firstWait).toBeLessThan(firstNoise);
  });
});
