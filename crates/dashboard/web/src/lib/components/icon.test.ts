// Icon contract: every cataloged icon renders as a stroke-based 24×24 svg,
// decorative unless labeled — a labeled icon must be exposed to AT, an
// unlabeled one must be hidden from it.
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import Icon from './Icon.svelte';
import { ICON_NAMES, ICON_PATHS } from '$lib/icons';

describe('Icon', () => {
  it('renders every cataloged icon with its path data', () => {
    for (const name of ICON_NAMES) {
      const { container, unmount } = render(Icon, { props: { name } });
      const svg = container.querySelector(`svg[data-icon="${name}"]`)!;
      expect(svg, name).not.toBeNull();
      expect(svg.getAttribute('viewBox')).toBe('0 0 24 24');
      expect(svg.getAttribute('stroke')).toBe('currentColor');
      expect(svg.getAttribute('fill')).toBe('none');
      expect(svg.querySelectorAll('path')).toHaveLength(ICON_PATHS[name].length);
      unmount();
    }
  });

  it('is decorative by default: hidden from assistive tech', () => {
    const { container } = render(Icon, { props: { name: 'inbox' } });
    const svg = container.querySelector('svg')!;
    expect(svg.getAttribute('aria-hidden')).toBe('true');
    expect(svg.getAttribute('role')).toBeNull();
  });

  it('speaks when labeled: role img + aria-label, not hidden', () => {
    const { container } = render(Icon, { props: { name: 'send', label: 'Send' } });
    const svg = container.querySelector('svg')!;
    expect(svg.getAttribute('role')).toBe('img');
    expect(svg.getAttribute('aria-label')).toBe('Send');
    expect(svg.getAttribute('aria-hidden')).toBeNull();
  });

  it('sizes from the size prop', () => {
    const { container } = render(Icon, { props: { name: 'clock', size: 20 } });
    const svg = container.querySelector('svg')!;
    expect(svg.getAttribute('width')).toBe('20');
    expect(svg.getAttribute('height')).toBe('20');
  });
});
