// Vitest stub for SvelteKit's `$app/state`. Exposes a mutable `page` reactive
// object the tests can mutate to drive route params/url. Components read
// `page.params` / `page.url`; assigning fresh values before render is enough.
//
// The fields are runes-backed so a test can also change the route on a MOUNTED
// component and have `$derived`/`$effect` re-run — that is what makes
// navigation races (a load still in flight when the route changes) testable.
// Runes require the `.svelte.ts` extension; the alias in vitest.config.ts
// points here.

class PageStub {
  params = $state<Record<string, string>>({});
  url = $state(new URL('http://localhost/v2/mail/unified'));
}

export const page = new PageStub();

export const navigating = null;
export const updated = { current: false };
