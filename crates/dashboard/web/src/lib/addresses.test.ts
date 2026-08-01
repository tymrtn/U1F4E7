// Unit tests for the shared recipient-address rules.
//
// Both send surfaces (the composer drawer and the draft review composer) gate
// on these, so the required/optional distinction is a safety boundary: a blank
// Cc is fine, a malformed one must never reach SMTP.

import { describe, expect, it } from 'vitest';
import { normalizedAddress, isValidEmail, parseAddrs, validateAddrs, optionalAddrsValid } from './addresses';

describe('normalizedAddress', () => {
  it('strips a display name', () => {
    expect(normalizedAddress('Ada Lovelace <ada@example.com>')).toBe('ada@example.com');
  });
  it('passes a bare address through', () => {
    expect(normalizedAddress('  ada@example.com ')).toBe('ada@example.com');
  });
});

describe('isValidEmail', () => {
  it('accepts ordinary and display-name addresses', () => {
    expect(isValidEmail('ada@example.com')).toBe(true);
    expect(isValidEmail('Ada <ada@example.com>')).toBe(true);
  });
  it('rejects malformed addresses', () => {
    for (const bad of ['', 'ada', 'ada@', '@example.com', 'ada@example', 'a b@example.com']) {
      expect(isValidEmail(bad)).toBe(false);
    }
  });
});

describe('parseAddrs', () => {
  it('splits on commas and drops blanks', () => {
    expect(parseAddrs('a@x.io, , b@x.io ')).toEqual(['a@x.io', 'b@x.io']);
  });
  it('returns nothing for a blank value', () => {
    expect(parseAddrs('   ')).toEqual([]);
  });
});

describe('validateAddrs (required header)', () => {
  it('requires at least one entry', () => {
    expect(validateAddrs('')).toBe(false);
    expect(validateAddrs('   ')).toBe(false);
  });
  it('requires every entry to be usable', () => {
    expect(validateAddrs('a@x.io, b@x.io')).toBe(true);
    expect(validateAddrs('a@x.io, broken')).toBe(false);
  });
});

describe('optionalAddrsValid (Cc/Bcc)', () => {
  it('treats blank as valid — these headers are optional', () => {
    expect(optionalAddrsValid('')).toBe(true);
    expect(optionalAddrsValid('   ')).toBe(true);
  });
  it('validates anything actually typed', () => {
    expect(optionalAddrsValid('a@x.io')).toBe(true);
    expect(optionalAddrsValid('a@x.io, b@x.io')).toBe(true);
    expect(optionalAddrsValid('broken')).toBe(false);
    expect(optionalAddrsValid('a@x.io, broken')).toBe(false);
  });
});
