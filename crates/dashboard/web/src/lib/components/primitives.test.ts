import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import PrimitiveHarness from './primitives.harness.svelte';

// Load the shipped route/rail shells as raw source at build time (Vite `?raw`),
// so the tripwire can scan them without node:fs and without @types/node.
const SHELL_SOURCES = import.meta.glob(
  ['../../routes/**/+page.svelte', '../../routes/**/+layout.svelte', './Rail.svelte'],
  { query: '?raw', import: 'default', eager: true }
) as Record<string, string>;

// Guards against v1's status-leak-as-label bug class: no rendered accessible
// name or visible text should surface backend telemetry phrasing OR the
// "wiring lands later" placeholder copy that leaked into the v1 rail footer.
const STATUS_LEAK =
  /aggregate view ships|cached index|placeholder|next wave|not.wired|coming soon/i;

describe('primitive tripwire — no status leaks as labels', () => {
  it('renders every primitive with representative props', () => {
    const { container } = render(PrimitiveHarness);
    const text = container.textContent ?? '';
    // Sanity: the harness actually rendered content.
    expect(text).toContain('Send');
    expect(text).toContain('Delivered');
    expect(text).toContain('uid 38103');

    // No leak phrases in visible text.
    expect(text).not.toMatch(STATUS_LEAK);

    // No accessible names (aria-label) leak either.
    const labelled = container.querySelectorAll('[aria-label]');
    expect(labelled.length).toBeGreaterThan(0);
    for (const el of labelled) {
      expect(el.getAttribute('aria-label') ?? '').not.toMatch(STATUS_LEAK);
    }
  });

  // Scan the actual route/rail shells too — the v1 bug lived in a rail footer
  // string, not a primitive. Cheap static source scan across the shipped
  // shells catches the phrasing before it reaches a screenshot.
  it('route/rail shells carry no placeholder status-leak copy', () => {
    const entries = Object.entries(SHELL_SOURCES);
    expect(entries.length).toBeGreaterThan(0);
    for (const [path, src] of entries) {
      // Strip HTML/line/block comments so an explanatory code comment (e.g. the
      // reader's "lands next wave" safety note) doesn't trip the guard, and
      // strip `placeholder=` input attributes (legitimate form UX, not the v1
      // stub-copy bug class we're guarding against) — only user-visible prose
      // is under test.
      const stripped = src
        .replace(/<!--[\s\S]*?-->/g, '')
        .replace(/\/\/.*$/gm, '')
        .replace(/\/\*[\s\S]*?\*\//g, '')
        .replace(/placeholder=(["'])[\s\S]*?\1/g, '')
        .replace(/placeholder=\{[^}]*\}/g, '');
      expect(stripped, `${path} leaks placeholder copy`).not.toMatch(STATUS_LEAK);
    }
  });
});
