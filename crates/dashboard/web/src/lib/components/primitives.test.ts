import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import PrimitiveHarness from './primitives.harness.svelte';

// Guards against v1's status-leak-as-label bug class: no rendered accessible
// name or visible text should surface backend telemetry phrasing.
const STATUS_LEAK = /not.available|wired|aggregate view ships|cached index/i;

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
});
