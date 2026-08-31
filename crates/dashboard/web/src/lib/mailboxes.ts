// GTD-stage catalog for the v2 rail (design plan rev 3). Each box is a client
// route under /v2/mail/<slug>; a box with `wired: false` renders an honest
// "not yet wired" empty state (no fake data), per the dashboard "surface
// explicit follow-up states" invariant.
//
// The rail groups these as GTD stages: `process` is the loop you drive to
// zero (capture → clarify → decide), `working` holds mail you're producing.
// Review surfaces (Cockpit, Approvals) are routes of their own, not boxes.
import type { IconName } from './icons';

export interface Mailbox {
  slug: string;
  label: string;
  icon: IconName;
  group: 'process' | 'working';
  /** True once this box loads real messages; gates the list vs. empty state. */
  wired: boolean;
}

export const MAILBOXES: Mailbox[] = [
  { slug: 'unified', label: 'Inbox', icon: 'inbox', group: 'process', wired: true },
  { slug: 'today', label: 'Today', icon: 'zap', group: 'process', wired: false },
  { slug: 'waiting-on', label: 'Waiting On', icon: 'hourglass', group: 'process', wired: false },
  { slug: 'snoozed', label: 'Snoozed', icon: 'clock', group: 'process', wired: true },
  { slug: 'scheduled', label: 'Scheduled', icon: 'calendar', group: 'process', wired: false },
  { slug: 'reference', label: 'Reference', icon: 'archive', group: 'process', wired: false },
  { slug: 'drafts', label: 'Drafts', icon: 'pen-line', group: 'working', wired: true },
  { slug: 'sent', label: 'Sent', icon: 'send', group: 'working', wired: true }
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
