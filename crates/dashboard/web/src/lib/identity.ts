// Identity presentation helpers: initials for an avatar from a display name
// or a bare address. Color lives in hue.ts; this is the text side.

/**
 * Two-letter initials for an identity label. Handles "First Last" names,
 * "Display Name <addr>" pairs, and bare addresses (falling back to the
 * local part). Unicode-safe: uses code points, not UTF-16 units, so an
 * accented or non-Latin first letter is never split.
 */
export function initials(nameOrAddr: string): string {
  const raw = (nameOrAddr ?? '').trim();
  if (!raw) return '?';

  // "Maria Keller <maria@x>" → "Maria Keller"; a bare "<addr>" keeps the addr.
  const stripped = raw.replace(/\s*<[^>]*>\s*$/, '').trim();
  const name = stripped || raw.replace(/[<>]/g, '').trim();
  if (!name) return '?';

  const firstCP = (s: string): string => Array.from(s)[0] ?? '';

  // A bare address (no spaces, has @): initial from the local part.
  if (!name.includes(' ') && name.includes('@')) {
    const local = name.slice(0, name.indexOf('@')).replace(/[._+-]+/g, ' ').trim();
    const toks = local.split(/\s+/).filter(Boolean);
    if (toks.length >= 2) return (firstCP(toks[0]) + firstCP(toks[1])).toUpperCase();
    const cps = Array.from(local);
    return (cps.slice(0, 2).join('') || '?').toUpperCase();
  }

  const toks = name.split(/\s+/).filter(Boolean);
  if (toks.length >= 2) {
    return (firstCP(toks[0]) + firstCP(toks[toks.length - 1])).toUpperCase();
  }
  const cps = Array.from(toks[0] ?? '');
  return (cps.slice(0, 2).join('') || '?').toUpperCase();
}
