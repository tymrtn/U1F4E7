import { redirect } from '@sveltejs/kit';
import { base } from '$app/paths';

// The v2 shell opens on the Unified Inbox. Redirect the bare /v2 root to the
// canonical mailbox route so every landing is a real, deep-linkable URL.
export function load() {
  redirect(307, `${base}/mail/unified`);
}
