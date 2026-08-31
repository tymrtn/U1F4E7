import { redirect } from '@sveltejs/kit';
import { base } from '$app/paths';

// The v2 shell opens on the Digest board — the GTD clarify surface (design
// plan rev 3). Redirect the bare root to the canonical route so every
// landing is a real, deep-linkable URL.
export function load() {
  redirect(307, `${base}/digest`);
}
