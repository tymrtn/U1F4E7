// Smart-mailbox catalog for the v2 rail. Each box is a client route under
// /v2/mail/<slug>; only Unified Inbox currently has a wired message list —
// the others render an honest "not yet wired" empty state (no fake data),
// per the dashboard "surface explicit follow-up states" invariant.

export interface Mailbox {
  slug: string;
  label: string;
  /** True once this box loads real messages; gates the list vs. empty state. */
  wired: boolean;
}

export const MAILBOXES: Mailbox[] = [
  { slug: 'unified', label: 'Unified Inbox', wired: true },
  { slug: 'needs-attention', label: 'Needs Attention', wired: false },
  { slug: 'snoozed', label: 'Snoozed', wired: true },
  { slug: 'drafts', label: 'Drafts', wired: true },
  { slug: 'sent', label: 'Sent', wired: false }
];

export function mailboxBySlug(slug: string): Mailbox | undefined {
  return MAILBOXES.find((m) => m.slug === slug);
}
