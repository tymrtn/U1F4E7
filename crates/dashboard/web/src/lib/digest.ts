// Digest board model (design plan rev 3, §4a). The digest is the GTD clarify
// surface: the agent categorizes, the human decides in bulk. Today only the
// capture bucket is wireable — the unified inbox payload carries no category
// data — so the section catalog ships with every category `wired: false` and
// the page renders honest awaiting-backend states, never fake groupings. The
// Phase E spike scopes the categorize backend that flips these on.
import type { UnifiedInboxMessage } from './api';

export type DigestTone = 'do' | 'wait' | 'noise';

export interface DigestSection {
  key: string;
  label: string;
  /** Drives the section edge-bar color: do = warn, wait = pending, noise = muted. */
  tone: DigestTone;
  /** True once a backend actually assigns threads to this category. */
  wired: boolean;
  /** Noise tiers get per-section bulk actions once wired. */
  bulk: boolean;
}

/** Shipped default taxonomy, in attention order. Rule-driven later. */
export const DIGEST_SECTIONS: DigestSection[] = [
  { key: 'reply-needed', label: 'Reply needed', tone: 'do', wired: false, bulk: false },
  { key: 'decision-needed', label: 'Decision needed', tone: 'do', wired: false, bulk: false },
  { key: 'todo', label: 'TODO', tone: 'do', wired: false, bulk: false },
  { key: 'awaiting-reply', label: 'Sent / awaiting reply', tone: 'wait', wired: false, bulk: false },
  { key: 'cold-outreach', label: 'Cold outreach + event invites', tone: 'noise', wired: false, bulk: true },
  { key: 'fyi', label: 'FYI / read', tone: 'noise', wired: false, bulk: true },
  { key: 'marketing', label: 'Marketing / spam / phishing', tone: 'noise', wired: false, bulk: true },
  { key: 'notifications', label: 'In-product notifications', tone: 'noise', wired: false, bulk: true }
];

/** One digest row: a thread, represented by its newest loaded message. */
export interface DigestThread {
  /** Stable row key: thread id when the index has one, account:uid otherwise. */
  key: string;
  subject: string;
  from: string;
  date: string | null;
  /** Loaded messages in this thread (a lower bound — paging truncates). */
  count: number;
  unread: boolean;
  accountId: string;
  uid: number;
  folder: string;
  messageId: string | null;
}

/**
 * Group a unified-inbox page (newest first) into threads, preserving order by
 * each thread's newest message. Messages without a thread id stand alone.
 */
export function groupIntoThreads(messages: UnifiedInboxMessage[]): DigestThread[] {
  const byKey = new Map<string, DigestThread>();
  for (const m of messages) {
    const key = m.thread_id ?? `${m.account_id}:${m.uid}`;
    const existing = byKey.get(key);
    if (existing) {
      existing.count += 1;
      existing.unread = existing.unread || m.unread;
    } else {
      byKey.set(key, {
        key,
        subject: m.subject ?? '',
        from: m.from_addr ?? '',
        date: m.date ?? null,
        count: 1,
        unread: m.unread,
        accountId: m.account_id,
        uid: m.uid,
        folder: m.folder,
        messageId: m.message_id ?? null
      });
    }
  }
  return [...byKey.values()];
}
