// Identity hue: deterministic color from an account id / sender address.
// Pure function of the key — no stored mapping, so the same identity renders
// the same hue in every session, list, and pane. Filled identity surfaces
// (initials avatars, account ticks) carry white text, so identityColor solves
// lightness per hue until the fill passes WCAG AA (4.5:1) against white —
// yellows need to sit much darker than blues to clear the same bar.

const HUE_SATURATION = 42;
const LIGHTNESS_START = 42;
const LIGHTNESS_FLOOR = 20;
const AA_CONTRAST = 4.5;

/** FNV-1a 32-bit over UTF-16 code units — stable, tiny, good spread. */
function fnv1a(key: string): number {
  let hash = 0x811c9dc5;
  for (let i = 0; i < key.length; i++) {
    hash ^= key.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

/** Deterministic hue in [0, 360) for an identity key. */
export function identityHue(key: string): number {
  return fnv1a(key) % 360;
}

function srgbChannel(c: number): number {
  const s = c / 255;
  return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
}

function hslLuminance(h: number, s: number, l: number): number {
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
  const r = Math.round((rgb[0] + m) * 255);
  const g = Math.round((rgb[1] + m) * 255);
  const b = Math.round((rgb[2] + m) * 255);
  return 0.2126 * srgbChannel(r) + 0.7152 * srgbChannel(g) + 0.0722 * srgbChannel(b);
}

function contrastVsWhite(h: number, s: number, l: number): number {
  return 1.05 / (hslLuminance(h, s, l) + 0.05);
}

/**
 * Fill color for an identity surface with white text on top.
 * Lightness walks down from LIGHTNESS_START until the hue clears AA against
 * white; the floor is a backstop the solver never actually needs.
 */
export function identityColor(key: string, hueOverride?: number): string {
  const hue = hueOverride ?? identityHue(key);
  let lightness = LIGHTNESS_START;
  while (lightness > LIGHTNESS_FLOOR && contrastVsWhite(hue, HUE_SATURATION, lightness) < AA_CONTRAST) {
    lightness -= 1;
  }
  return `hsl(${hue} ${HUE_SATURATION}% ${lightness}%)`;
}
