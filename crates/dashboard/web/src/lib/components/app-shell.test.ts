// Root app shell (`src/routes/+layout.svelte`) — brand version + nav.
//
// Regression guard for the hard-coded brand tag: the shell used to print a
// literal `v1.0.0` no matter which binary was actually serving the dashboard,
// so a stale launchd service (the exact drift /api/health exists to expose)
// silently claimed a version it wasn't running. The tag must now echo
// `/api/health.version` verbatim, and show nothing at all when it can't.
//
// These tests drive the REAL api client (no `$lib/api` mock) and stub global
// fetch, so the whole chain — component → api.health() → request() →
// GET /api/health — is under test.

import { render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { createRawSnippet } from 'svelte';
import AppShell from '../../routes/+layout.svelte';

// Raw source of the shell, so the "no baked-in release string" assertion reads
// the shipped file rather than only the rendered output (same `?raw` glob
// trick the primitive tripwire uses — avoids node:fs and @types/node).
const SHELL_SOURCE = Object.values(
  import.meta.glob('../../routes/+layout.svelte', {
    query: '?raw',
    import: 'default',
    eager: true
  })
)[0] as string;

/** A trivial snippet to satisfy the layout's `children` slot. */
const children = createRawSnippet(() => ({ render: () => '<span>route content</span>' }));

/** Minimal Response-like object for a mocked fetch (mirrors api.test.ts). */
function jsonResponse(body: unknown, init: { status?: number } = {}): Response {
  const status = init.status ?? 200;
  const payload = JSON.stringify(body);
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => JSON.parse(payload),
    clone() {
      return jsonResponse(body, init);
    }
  } as unknown as Response;
}

function stubFetch(impl: (url: RequestInfo | URL) => Promise<Response>) {
  const fetchMock = vi.fn(impl);
  vi.stubGlobal('fetch', fetchMock);
  return fetchMock;
}

/** Health payload as the unauthenticated handler returns it. */
function healthOk(version: string) {
  return jsonResponse({ status: 'ok', service: 'envelope-dashboard', version });
}

function brandTag(container: HTMLElement): HTMLElement | null {
  return container.querySelector('.brand-tag');
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('app shell brand version', () => {
  it('renders the version reported by /api/health', async () => {
    const fetchMock = stubFetch(async () => healthOk('1.0.3'));

    const { container } = render(AppShell, { props: { children } });

    await waitFor(() => expect(brandTag(container)?.textContent).toBe('v1.0.3'));
    expect(screen.getByText('v1.0.3')).toBeInTheDocument();
    expect(String(fetchMock.mock.calls[0]![0])).toBe('/api/health');
    // The old baked-in string must never appear, whatever the backend says.
    expect(container.textContent).not.toContain('v1.0.0');
  });

  it('echoes an arbitrary backend version verbatim', async () => {
    stubFetch(async () => healthOk('9.4.2-rc7'));

    const { container } = render(AppShell, { props: { children } });

    await waitFor(() => expect(brandTag(container)?.textContent).toBe('v9.4.2-rc7'));
  });

  it('shows no version while /api/health is still in flight', async () => {
    // Never-resolving fetch: the loading state must not invent a release.
    stubFetch(() => new Promise<Response>(() => {}));

    const { container } = render(AppShell, { props: { children } });

    expect(brandTag(container)).toBeNull();
    expect(container.textContent).not.toMatch(/v\d+\.\d+\.\d+/);
  });

  it('shows no version when /api/health fails', async () => {
    const fetchMock = stubFetch(async () => jsonResponse({ code: 'boom' }, { status: 500 }));

    const { container } = render(AppShell, { props: { children } });

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    // Let the rejected request settle through the component's catch.
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(brandTag(container)).toBeNull();
    expect(container.textContent).not.toContain('v1.0.0');
    expect(container.textContent).not.toMatch(/v\d+\.\d+\.\d+/);
    // The shell itself still works — a failed version probe is not fatal.
    expect(screen.getByText('Envelope')).toBeInTheDocument();
    expect(screen.getByText('route content')).toBeInTheDocument();
  });

  it('shows no version when the payload carries no version field', async () => {
    const fetchMock = stubFetch(async () =>
      jsonResponse({ status: 'ok', service: 'envelope-dashboard' })
    );

    const { container } = render(AppShell, { props: { children } });

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(brandTag(container)).toBeNull();
  });

  it('carries no hard-coded release string in the shell source', () => {
    expect(SHELL_SOURCE.length).toBeGreaterThan(0);
    expect(SHELL_SOURCE).toMatch(/api\.health\(\)/);
    // Strip comments first: prose explaining the old `v1.0.0` bug is fine, a
    // literal version in markup is not.
    const stripped = SHELL_SOURCE.replace(/<!--[\s\S]*?-->/g, '')
      .replace(/\/\/.*$/gm, '')
      .replace(/\/\*[\s\S]*?\*\//g, '');
    expect(stripped).not.toMatch(/v\d+\.\d+\.\d+/);
  });
});

describe('app shell layout', () => {
  it('keeps the brand link and primary nav intact', async () => {
    stubFetch(async () => healthOk('1.0.3'));

    render(AppShell, { props: { children } });

    // base is '/v2' in the test stub (src/test-stubs/app-paths.ts).
    expect(screen.getByRole('link', { name: /Envelope/ })).toHaveAttribute('href', '/v2');
    const nav = screen.getByRole('navigation', { name: 'Primary navigation' });
    expect(nav).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Mail' })).toHaveAttribute('href', '/v2/mail/unified');
    expect(screen.getByRole('link', { name: 'Review' })).toHaveAttribute('href', '/v2/review');
    expect(screen.getByRole('link', { name: 'Cockpit' })).toHaveAttribute('href', '/v2/cockpit');
    expect(screen.getByRole('link', { name: 'Rules' })).toHaveAttribute('href', '/v2/rules');
    // The daily Review queue outranks the diagnostic Cockpit in nav order.
    const labels = Array.from(nav.querySelectorAll('a')).map((a) => a.textContent?.trim());
    expect(labels).toEqual(['Mail', 'Review', 'Cockpit', 'Rules']);
    // The stub page url is /v2/mail/unified, so Mail is the active tab.
    expect(screen.getByRole('link', { name: 'Mail' })).toHaveClass('is-active');
    expect(screen.getByRole('link', { name: 'Cockpit' })).not.toHaveClass('is-active');
  });

  it('sizes the mobile nav grid to the actual tab count', async () => {
    stubFetch(async () => healthOk('1.0.3'));

    const { container } = render(AppShell, { props: { children } });

    // The 640px breakpoint lays tabs out as `repeat(N, minmax(0, 1fr))`. A
    // hard-coded N smaller than the tab count silently wraps the last tab onto
    // a second row — keep the declared column count in lockstep.
    const match = SHELL_SOURCE.match(/repeat\((\d+), minmax\(0, 1fr\)\)/);
    expect(match).not.toBeNull();
    const columns = Number(match![1]);
    const tabCount = container.querySelectorAll('.app-nav a').length;
    expect(columns).toBe(tabCount);
  });

  it('renders route children inside the main region', async () => {
    stubFetch(async () => healthOk('1.0.3'));

    const { container } = render(AppShell, { props: { children } });

    expect(container.querySelector('.app-main')?.textContent).toContain('route content');
  });
});
