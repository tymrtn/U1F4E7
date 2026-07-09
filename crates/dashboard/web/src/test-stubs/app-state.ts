// Vitest stub for SvelteKit's `$app/state`. Exposes a mutable `page` reactive
// object the tests can mutate to drive route params/url. Components read
// `page.params` / `page.url`; assigning fresh values before render is enough.
export const page: {
  params: Record<string, string>;
  url: URL;
} = {
  params: {},
  url: new URL('http://localhost/v2/mail/unified')
};

export const navigating = null;
export const updated = { current: false };
