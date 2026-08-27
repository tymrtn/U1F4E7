// Smart-mailbox catalog for the v2 rail. Each box is a client route under
// /v2/mail/<slug>; an unwired box renders an honest "not yet wired" empty
// state (no fake data), per the dashboard "surface explicit follow-up
// states" invariant.

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
  { slug: 'sent', label: 'Sent', wired: true }
];

export function mailboxBySlug(slug: string): Mailbox | undefined {
  return MAILBOXES.find((m) => m.slug === slug);
}

/**
 * IMAP folder names that the backend's `provider::classify_folder` returns
 * `"drafts"` for. Kept in sync by hand with crates/email/src/provider.rs — a
 * name missing here sends a draft deep link to the read-only reader, which has
 * no Send.
 */
const DRAFTS_FOLDERS = new Set([
  'drafts',
  '[gmail]/drafts',
  'draft',
  'inbox.drafts',
  'inbox/drafts',
  'inbox/draft'
]);

/** True when `name` is a Drafts mailbox, matching the backend classification. */
export function isDraftsFolder(name: string | null | undefined): boolean {
  if (!name) return false;
  return DRAFTS_FOLDERS.has(name.toLowerCase());
}

/**
 * IMAP folder names that `provider::classify_folder` returns `"sent"` for.
 * Kept in sync by hand with crates/email/src/provider.rs — the Sent smart
 * mailbox resolves each account's real Sent folder through this set, so a
 * name missing here silently drops that account from the Sent list.
 */
const SENT_FOLDERS = new Set([
  'sent',
  'sent mail',
  'sent messages',
  'sent items',
  '[gmail]/sent mail',
  'inbox.sent',
  'inbox.sent messages',
  'inbox/sent',
  'inbox/sent mail',
  'inbox/sent messages',
  'inbox/sent items'
]);

/** True when `name` is a Sent mailbox, matching the backend classification. */
export function isSentFolder(name: string | null | undefined): boolean {
  if (!name) return false;
  return SENT_FOLDERS.has(name.toLowerCase());
}
