import { describe, expect, it } from 'vitest';

import { isSafeLinkUrl, sanitizeEmailHtml } from './rich-html';
import fixtures from './sanitize-fixtures.json';

interface SanitizeFixture {
  name: string;
  input: string;
  must_contain: string[];
  must_not_match: string[];
}

// The same cases run against the server-side sanitizer
// (crates/email/src/sanitize.rs), so the two policies cannot drift apart.
describe('shared sanitizer fixtures', () => {
  for (const fixture of fixtures.cases as SanitizeFixture[]) {
    it(fixture.name, () => {
      const html = sanitizeEmailHtml(fixture.input, {
        remoteImages: false,
        externalLinkTargets: false
      }).html;
      for (const needle of fixture.must_contain) expect(html).toContain(needle);
      for (const pattern of fixture.must_not_match) expect(html).not.toMatch(new RegExp(pattern, 'i'));
    });
  }
});

describe('rich HTML policy', () => {
  it('removes active elements, event handlers, embeds, and navigation attributes', () => {
    const escapedCss = String.raw`<p style='background-image:\69 mage-set("\68 \74 \74 \70 \73 \3a \2f \2f tracker.example/pixel.png" 1x)'>escaped-css</p>`;
    const result = sanitizeEmailHtml(
      '<p onmouseover="evil()" autofocus tabindex="0">Safe</p>' +
        '<script>alert(1)</script><form action="/send"><input></form>' +
        '<object data="https://evil.example"></object><embed src="https://evil.example">' +
        '<svg><a xlink:href="javascript:evil()">vector</a></svg>' +
        '<template shadowrootmode="open"><script>evil()</script><img src="https://tracker.example/t"></template>' +
        '<p style="background-image:image-set(&quot;https://tracker.example/pixel&quot; 1x)">css</p>' +
        escapedCss +
        '<a xlink:href="https://example.com/safe">safe-vector</a>' +
        '<a href="java&#x0A;script:evil()" download ping="https://tracker.example">bad</a>',
      { remoteImages: true, externalLinkTargets: false }
    ).html;

    expect(result).toBe('<p>Safe</p><p>css</p><p>escaped-css</p><a>safe-vector</a><a>bad</a>');
    expect(result).not.toMatch(/script|form|object|embed|svg|xlink|template|image-set|onmouseover|autofocus|tabindex|download|ping/i);
  });

  it('uses a parsed protocol allowlist and rejects control-character obfuscation', () => {
    expect(isSafeLinkUrl('https://example.com/path')).toBe(true);
    expect(isSafeLinkUrl('mailto:person@example.com')).toBe(true);
    expect(isSafeLinkUrl('tel:+15551212')).toBe(true);
    expect(isSafeLinkUrl('#section')).toBe(true);
    expect(isSafeLinkUrl('java\nscript:alert(1)')).toBe(false);
    expect(isSafeLinkUrl('vbscript:msgbox(1)')).toBe(false);
    expect(isSafeLinkUrl('data:text/html,<script>alert(1)</script>')).toBe(false);
    expect(isSafeLinkUrl('/relative/navigation')).toBe(false);
  });

  it('serializes only sanitized body roots and blocks SVG data image payloads', () => {
    const result = sanitizeEmailHtml(
      '<html><head><title>not body</title></head><body><p>Body</p>' +
        '<img src="data:image/svg+xml,%3Csvg%20onload=evil()%3E">' +
        '<img src="cid:logo"></body></html>',
      { remoteImages: true, externalLinkTargets: false }
    ).html;

    expect(result).toBe('<p>Body</p><img><img src="cid:logo">');
    expect(result).not.toMatch(/<html|<head|<body|env-editor|data:image\/svg/i);
  });

  it('makes remote images inert for editing and restores them for serialization', () => {
    const inert = sanitizeEmailHtml(
      '<img src="https://tracker.example/pixel.gif" alt="remote">',
      {
        remoteImages: true,
        externalLinkTargets: false,
        inertImages: true
      }
    ).html;
    expect(inert).toContain('data-env-src="https://tracker.example/pixel.gif"');
    expect(inert).not.toContain(' src="https://tracker.example/pixel.gif"');

    const serialized = sanitizeEmailHtml(inert, {
      remoteImages: true,
      externalLinkTargets: false
    }).html;
    expect(serialized).toContain('src="https://tracker.example/pixel.gif"');
    expect(serialized).not.toContain('data-env-src');
  });

  it('can make links inert for editing and restore them for serialization', () => {
    const inert = sanitizeEmailHtml('<a href="https://example.com">safe</a>', {
      remoteImages: true,
      externalLinkTargets: false,
      inertLinks: true
    }).html;
    expect(inert).toBe(
      '<a data-env-href="https://example.com" tabindex="-1">safe</a>'
    );

    const serialized = sanitizeEmailHtml(inert, {
      remoteImages: true,
      externalLinkTargets: false
    }).html;
    expect(serialized).toBe('<a href="https://example.com">safe</a>');
  });
});
