import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import Avatar from './Avatar.svelte';
import { identityColor } from '$lib/hue';

describe('Avatar', () => {
  it('renders initials from the name', () => {
    const { container } = render(Avatar, { props: { name: 'Maria Keller' } });
    const el = container.querySelector('.avatar')!;
    expect(el.getAttribute('data-initials')).toBe('MK');
    expect(el.textContent?.trim()).toBe('MK');
  });

  it('is decorative — hidden from assistive tech', () => {
    const { container } = render(Avatar, { props: { name: 'Maria Keller' } });
    expect(container.querySelector('.avatar')!.getAttribute('aria-hidden')).toBe('true');
  });

  it('fills with the identity color of hueKey, not name, when given', () => {
    const { container } = render(Avatar, {
      props: { name: 'Work Mail', hueKey: 'acct-work' }
    });
    const style = container.querySelector('.avatar')!.getAttribute('style') ?? '';
    expect(style).toContain(identityColor('acct-work'));
  });

  it('ships without the CRM ring by default (initials-only at launch)', () => {
    const { container } = render(Avatar, { props: { name: 'Maria Keller' } });
    expect(container.querySelector('.avatar')!.classList.contains('has-ring')).toBe(false);
  });

  it('shows the ring when explicitly enabled', () => {
    const { container } = render(Avatar, { props: { name: 'Maria Keller', ring: true } });
    expect(container.querySelector('.avatar')!.classList.contains('has-ring')).toBe(true);
  });
});
