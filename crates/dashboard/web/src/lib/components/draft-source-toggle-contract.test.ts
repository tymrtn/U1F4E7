// The "Edit HTML source" / "Rich text" toggle on the draft composer toolbar.
//
// It shipped as a bare <button> painted ink-on-surface with no padding or
// border, so it read as a highlighted word rather than a control. This
// contract keeps it a real button styled like the Text/HTML segment next to
// it: bordered and neutral at rest, inverted only while pressed (source mode).
/// <reference types="vite/client" />
import { describe, expect, it } from 'vitest';

import composerSource from './DraftComposer.svelte?raw';

function styleOf(source: string): string {
  const start = source.lastIndexOf('<style>');
  const end = source.lastIndexOf('</style>');
  if (start === -1 || end <= start) throw new Error('component has no <style> block');
  return source.slice(start + '<style>'.length, end).replace(/\/\*[\s\S]*?\*\//g, '');
}

/** Declarations of the first rule whose selector list is exactly `selector`. */
function ruleFor(css: string, selector: string): string {
  const pattern = new RegExp(
    `(^|[}])\\s*${selector.replace(/[.[\]()']/g, '\\$&')}\\s*\\{([^}]*)\\}`
  );
  const match = css.match(pattern);
  if (!match) throw new Error(`missing rule for selector: ${selector}`);
  return match[2];
}

const css = styleOf(composerSource);

describe('draft composer: HTML source toggle is a bordered button', () => {
  it('has a border, padding and a neutral surface at rest', () => {
    const rest = ruleFor(css, '.draft-preview-toggle');
    expect(rest).toMatch(/border:\s*1px solid var\(--env-rule\)/);
    expect(rest).toMatch(/padding:\s*0 /);
    expect(rest).toContain('background: var(--env-surface)');
    expect(rest).not.toContain('background: var(--env-ink)');
  });

  it('inverts only while pressed, matching the active format segment', () => {
    const pressed = ruleFor(css, ".draft-preview-toggle[aria-pressed='true']");
    expect(pressed).toContain('background: var(--env-ink)');
    expect(pressed).toContain('color: var(--env-surface)');
  });
});
