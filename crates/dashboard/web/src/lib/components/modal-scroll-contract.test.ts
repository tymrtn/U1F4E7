// Modal viewport contract (Governor context-refinement overflow regression).
//
// The bug: `Refine context` opens a dialog listing every catalog attribute.
// `.env-modal` declared no height ceiling and `.env-modal-body` no scroller, so
// on a backdrop that is `position: fixed` and vertically centred the dialog grew
// past both viewport edges. The heading and the "Facts you can correct"
// checkboxes were clipped off the top, the footer buttons off the bottom, and
// nothing scrolled — the dialog was unusable and uncompletable.
//
// jsdom performs no layout, so the contract is asserted against the component
// style block itself, in the manner of `mobile-scroll-contract.test.ts`.
/// <reference types="vite/client" />
import { describe, expect, it } from 'vitest';

import modalSource from './Modal.svelte?raw';

function styleOf(source: string): string {
  const start = source.lastIndexOf('<style>');
  const end = source.lastIndexOf('</style>');
  if (start === -1 || end <= start) throw new Error('component has no <style> block');
  return source.slice(start + '<style>'.length, end).replace(/\/\*[\s\S]*?\*\//g, '');
}

/** Declarations of the first rule whose selector list is exactly `selector`. */
function ruleFor(css: string, selector: string): string {
  const pattern = new RegExp(
    `(^|[}])\\s*${selector.replace(/[.[\]()]/g, '\\$&')}\\s*\\{([^}]*)\\}`
  );
  const match = css.match(pattern);
  if (!match) throw new Error(`missing rule for selector: ${selector}`);
  return match[2];
}

const modal = styleOf(modalSource);

describe('modal viewport contract', () => {
  it('clamps the dialog to the viewport', () => {
    expect(ruleFor(modal, '.env-modal')).toMatch(/max-height\s*:/);
  });

  it('gives the body the scroller, so head and foot stay reachable', () => {
    const body = ruleFor(modal, '.env-modal-body');
    expect(body).toMatch(/overflow-y\s*:\s*auto/);
    // A flex child will not shrink below its content without this, which is
    // what silently defeats the clamp above.
    expect(body).toMatch(/min-height\s*:\s*0/);
  });

  it('keeps head and foot out of the scroller', () => {
    expect(ruleFor(modal, '.env-modal-head')).toMatch(/flex\s*:\s*none/);
    expect(ruleFor(modal, '.env-modal-foot')).toMatch(/flex\s*:\s*none/);
  });
});
