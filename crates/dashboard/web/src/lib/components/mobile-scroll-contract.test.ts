// Mobile single-scroller contract (iPhone draft/reader scroll regression).
//
// The bug: on a narrow screen, a long forwarded HTML draft could not be
// scrolled past — the content below the rendered email was unreachable. The
// message iframe was sized correctly; the problem was WHO owned the scroll.
// Desktop uses internal pane scrollers (`.reader` / `.draft-review` with
// overflow auto). On iOS, a touch that starts on a tall sandboxed <iframe>
// belongs to the iframe and never chains out, so the page cannot be flicked
// past the email. The one-column mobile layout therefore hands scrolling to
// the document: panes get `overflow: visible`, heights grow with content, and
// BodyFrame forwards iframe touch deltas to that document scroller.
//
// jsdom applies neither media queries nor real layout, so this contract is
// asserted against the component styles themselves: the mobile media block
// must neutralize every pane scroller on the reader + draft-composer path,
// and the desktop (base) rules must keep theirs.
/// <reference types="vite/client" />
import { describe, expect, it } from 'vitest';

import shellSource from '../../routes/+layout.svelte?raw';
import mailSource from '../../routes/mail/[box]/+layout.svelte?raw';
import composerSource from './DraftComposer.svelte?raw';
import bodyFrameSource from './BodyFrame.svelte?raw';

const MOBILE_QUERY = '@media (max-width: 760px)';

function styleOf(source: string): string {
  // The LAST <style> tag is the component's own: BodyFrame's script also
  // builds a srcdoc string that contains literal <style> markup. Comments are
  // stripped so prose quoting a declaration can never satisfy (or trip) an
  // assertion about the declarations themselves.
  const start = source.lastIndexOf('<style>');
  const end = source.lastIndexOf('</style>');
  if (start === -1 || end <= start) throw new Error('component has no <style> block');
  return source.slice(start + '<style>'.length, end).replace(/\/\*[\s\S]*?\*\//g, '');
}

/** The body of `query { ... }`, found by brace matching. */
function mediaBlock(css: string, query: string): string {
  const start = css.indexOf(query);
  if (start === -1) throw new Error(`missing media query: ${query}`);
  const open = css.indexOf('{', start);
  let depth = 1;
  let i = open + 1;
  while (i < css.length && depth > 0) {
    if (css[i] === '{') depth += 1;
    else if (css[i] === '}') depth -= 1;
    i += 1;
  }
  return css.slice(open + 1, i - 1);
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

const shell = styleOf(shellSource);
const mail = styleOf(mailSource);
const composer = styleOf(composerSource);
const bodyFrame = styleOf(bodyFrameSource);

describe('mobile one-scroller contract: reader + draft composer', () => {
  it('releases the app shell viewport clamp at the layouts’ 760px breakpoint', () => {
    // Desktop keeps the fixed-viewport shell…
    expect(ruleFor(shell, '.app-shell')).toContain('height: 100vh');
    // …and the mobile release happens at 760px — NOT only at a narrower width.
    // Between the two breakpoints the panes would otherwise be forced back
    // onto their inner scrollers.
    const mobile = mediaBlock(shell, MOBILE_QUERY);
    const mobileShell = ruleFor(mobile, '.app-shell');
    expect(mobileShell).toContain('height: auto');
    expect(mobileShell).toContain('min-height: 100vh');
  });

  it('makes the reader pane a non-scroller in the one-column layout', () => {
    const mobile = mediaBlock(mail, MOBILE_QUERY);
    expect(ruleFor(mobile, '.mail-shell')).toContain('overflow: visible');
    const reader = ruleFor(mobile, '.mail-shell.is-reading .reader');
    expect(reader).toContain('overflow: visible');
    expect(reader).not.toMatch(/overflow[^;]*(auto|scroll|hidden)/);
    // No fixed-height clamp may reappear on the mobile reader.
    expect(reader).not.toMatch(/(^|[^-])(height|max-height)\s*:/m);
  });

  it('makes the draft review pane a non-scroller in the one-column layout', () => {
    const mobile = mediaBlock(composer, MOBILE_QUERY);
    const review = ruleFor(mobile, '.draft-review');
    expect(review).toContain('overflow: visible');
    expect(review).not.toMatch(/overflow[^;]*(auto|scroll|hidden)/);
  });

  it('keeps the preview chain free of inner scrollers and height wells', () => {
    // `.draft-preview` once trapped the message in a fixed-height well with
    // its own scrollbar; the frame now sizes to the document and the page
    // scrolls. Neither surface may reintroduce a clamp.
    const preview = ruleFor(composer, '.draft-preview');
    expect(preview).not.toMatch(/overflow|max-height|flex\s*:/);
    expect(ruleFor(bodyFrame, '.body-frame')).not.toMatch(/overflow/);
  });

  it('keeps the desktop HTML preview interactive', () => {
    expect(ruleFor(bodyFrame, '.body-frame')).not.toMatch(/pointer-events/);
  });

  it('keeps the mobile HTML preview interactive', () => {
    // The event bridge owns scrolling; no responsive rule may disable links,
    // quote toggles, text selection, focus, or taps on narrow screens.
    expect(bodyFrame).not.toMatch(/pointer-events\s*:\s*none/);
  });

  it('keeps the desktop three-pane internal scrollers', () => {
    expect(ruleFor(mail, '.reader')).toContain('overflow-y: auto');
    expect(ruleFor(mail, '.list')).toContain('overflow-y: auto');
    expect(ruleFor(composer, '.draft-review')).toContain('overflow: auto');
  });
});
