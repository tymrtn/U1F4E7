// Identity hue: deterministic color from an account id / sender address.
// Color is metadata — the same identity must render the same hue everywhere,
// forever, with no stored mapping. Filled identity surfaces (avatars, ticks)
// carry white initials, so every produced color must pass WCAG AA (4.5:1)
// against white regardless of which hue the hash lands on.
import { describe, expect, it } from 'vitest';
import { identityColor, identityHue } from './hue';

// --- WCAG relative-luminance contrast, per WCAG 2.x definition ---
function srgbChannel(c: number): number {
  const s = c / 255;
  return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
}
function luminance(r: number, g: number, b: number): number {
  return 0.2126 * srgbChannel(r) + 0.7152 * srgbChannel(g) + 0.0722 * srgbChannel(b);
}
function contrastVsWhite(r: number, g: number, b: number): number {
  return (1.0 + 0.05) / (luminance(r, g, b) + 0.05);
}
function hslToRgb(h: number, s: number, l: number): [number, number, number] {
  const sat = s / 100;
  const lig = l / 100;
  const c = (1 - Math.abs(2 * lig - 1)) * sat;
  const x = c * (1 - Math.abs(((h / 60) % 2) - 1));
  const m = lig - c / 2;
  let rgb: [number, number, number];
  if (h < 60) rgb = [c, x, 0];
  else if (h < 120) rgb = [x, c, 0];
  else if (h < 180) rgb = [0, c, x];
  else if (h < 240) rgb = [0, x, c];
  else if (h < 300) rgb = [x, 0, c];
  else rgb = [c, 0, x];
  return [
    Math.round((rgb[0] + m) * 255),
    Math.round((rgb[1] + m) * 255),
    Math.round((rgb[2] + m) * 255)
  ];
}
function parseHsl(color: string): { h: number; s: number; l: number } {
  const m = color.match(/^hsl\((\d+(?:\.\d+)?)\s+(\d+(?:\.\d+)?)%\s+(\d+(?:\.\d+)?)%\)$/);
  if (!m) throw new Error(`not an hsl() string: ${color}`);
  return { h: Number(m[1]), s: Number(m[2]), l: Number(m[3]) };
}

const SAMPLE_KEYS = [
  'tyler@tmrtn.com',
  'desk@memberdesk.example',
  'acc-work',
  'acc-home',
  'notifications@github.com',
  'newsletter@info.zoomadrid.com',
  'maria.keller@example.com',
  'bowei@imbue.example',
  'a',
  'b',
  'ab',
  'ba',
  'Ma Isabel Pérez — üñïçødé@example.es',
  'very-long-account-identifier-with-lots-of-entropy-0123456789@example.org'
];

describe('identityHue', () => {
  it('is deterministic: same key, same hue, every call', () => {
    for (const key of SAMPLE_KEYS) {
      expect(identityHue(key)).toBe(identityHue(key));
    }
  });

  it('stays in [0, 360)', () => {
    for (const key of SAMPLE_KEYS) {
      const h = identityHue(key);
      expect(h).toBeGreaterThanOrEqual(0);
      expect(h).toBeLessThan(360);
      expect(Number.isInteger(h)).toBe(true);
    }
  });

  it('spreads distinct keys across distinct hues', () => {
    const hues = new Set(SAMPLE_KEYS.map(identityHue));
    // A perfect spread is not required — collisions happen — but the sample
    // set must not clump: near-order-of-input distinct values.
    expect(hues.size).toBeGreaterThanOrEqual(SAMPLE_KEYS.length - 3);
  });

  it('treats the empty key as a valid identity', () => {
    const h = identityHue('');
    expect(h).toBeGreaterThanOrEqual(0);
    expect(h).toBeLessThan(360);
  });
});

describe('identityColor', () => {
  it('returns an hsl() string on the key’s own hue', () => {
    for (const key of SAMPLE_KEYS) {
      const { h } = parseHsl(identityColor(key));
      expect(h).toBe(identityHue(key));
    }
  });

  it('passes WCAG AA (4.5:1) against white initials for every key', () => {
    for (const key of SAMPLE_KEYS) {
      const { h, s, l } = parseHsl(identityColor(key));
      const [r, g, b] = hslToRgb(h, s, l);
      expect(contrastVsWhite(r, g, b)).toBeGreaterThanOrEqual(4.5);
    }
  });

  it('passes AA against white across the entire hue wheel, not just samples', () => {
    // Drive the lightness solver over every hue directly: synthetic keys are
    // not guaranteed to cover the yellow band, where white-on-color fails
    // soonest, so assert the wheel itself.
    for (let hue = 0; hue < 360; hue++) {
      const { s, l } = parseHsl(identityColor(`wheel-${hue}`, hue));
      const [r, g, b] = hslToRgb(hue, s, l);
      expect(contrastVsWhite(r, g, b)).toBeGreaterThanOrEqual(4.5);
    }
  });
});
