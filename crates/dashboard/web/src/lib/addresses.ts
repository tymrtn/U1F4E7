// Recipient-address parsing and validation, shared by every surface that can
// put mail on the wire.
//
// The composer drawer and the draft review composer have to agree on what
// counts as a usable recipient. If they drift, a draft that Compose would
// refuse to send can still be queued from the review page, and the empty
// address only surfaces at SMTP time.

/** Strip a display name, leaving the bare address: `Ada <a@x.io>` → `a@x.io`. */
export function normalizedAddress(addr: string): string {
  const angle = addr.match(/<([^>]+)>/);
  return (angle?.[1] ?? addr).trim();
}

export function isValidEmail(addr: string): boolean {
  return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(normalizedAddress(addr));
}

/** Split a comma-separated header value into its non-empty entries. */
export function parseAddrs(raw: string): string[] {
  return raw
    .split(',')
    .map((value) => value.trim())
    .filter(Boolean);
}

/** True when `raw` carries at least one entry and every entry is usable. */
export function validateAddrs(raw: string): boolean {
  const addrs = parseAddrs(raw);
  return addrs.length > 0 && addrs.every(isValidEmail);
}

/**
 * Validation for an OPTIONAL header (Cc/Bcc): blank is fine, but anything
 * actually typed has to be usable. Skipping this lets a malformed Cc ride
 * along on an otherwise valid send and fail at SMTP time.
 */
export function optionalAddrsValid(raw: string): boolean {
  return raw.trim() === '' || validateAddrs(raw);
}
