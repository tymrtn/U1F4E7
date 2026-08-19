// Unit tests for the shared recipient-address rules.
//
// Both send surfaces (the composer drawer and the draft review composer) gate
// on these, so the required/optional distinction is a safety boundary: a blank
// Cc is fine, a malformed one must never reach SMTP.

import { describe, expect, it } from 'vitest';
import {
  addrKey,
  addrLabel,
  formatAddr,
  normalizedAddress,
  isValidEmail,
  parseAddrs,
  serializeAddrs,
  validateAddrs,
  optionalAddrsValid
} from './addresses';

describe('normalizedAddress', () => {
  it('strips a display name', () => {
    expect(normalizedAddress('Ada Lovelace <ada@example.com>')).toBe('ada@example.com');
  });
  it('passes a bare address through', () => {
    expect(normalizedAddress('  ada@example.com ')).toBe('ada@example.com');
  });
  // The angle address has to CLOSE the entry. Extracting it with a substring
  // match found `<ada@example.com>` inside `Ada <ada@example.com> trailing` and
  // handed only that to the validator, so the entry passed every frontend gate
  // and then failed at `lettre::Mailboxes`, whose `mailbox_list` is eof-
  // terminated. Leaving the whole entry in place is what lets isValidEmail see
  // the leftover text and refuse it.
  it('keeps a malformed entry whole rather than mining an address out of it', () => {
    expect(normalizedAddress('Ada <ada@example.com> trailing')).toBe(
      'Ada <ada@example.com> trailing'
    );
    expect(normalizedAddress('<ada@example.com> <bob@example.com>')).toBe(
      '<ada@example.com> <bob@example.com>'
    );
    expect(normalizedAddress('Ada <ada@example.com')).toBe('Ada <ada@example.com');
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
  // The send path parses through lettre::Address, which rejects these. A
  // looser rule here would queue a draft that only fails at SMTP time.
  it('rejects addresses the send path would refuse', () => {
    for (const bad of [
      'a..b@example.com',
      '.ada@example.com',
      'ada.@example.com',
      'ada@-example.com',
      'ada@example-.com',
      'ada@exa..mple.com',
      'ada@.example.com',
      `${'a'.repeat(65)}@example.com`,
      `ada@${'a'.repeat(64)}.com`
    ]) {
      expect(isValidEmail(bad), bad).toBe(false);
    }
  });
  // RFC 5322 allows a quoted local part, and `lettre::Address::from_str`
  // accepts one — but recipient headers never go through that. Every To/Cc/Bcc
  // value is parsed by `lettre::Mailboxes`, which refuses them with
  // InvalidUser. Accepting one here would queue a draft that dies at SMTP.
  // Pinned against the send edge by
  // `parse_mailboxes_rejects_what_the_suggestion_and_composer_gates_also_reject`
  // in `crates/email/src/smtp.rs`.
  it('rejects quoted local parts, which the recipient headers cannot carry', () => {
    for (const bad of [
      '"john..doe"@example.com',
      '"john doe"@example.com',
      '"johndoe"@example.com',
      '""@example.com',
      '"unterminated@example.com',
      'unopened"@example.com',
      'John <"john..doe"@example.com>'
    ]) {
      expect(isValidEmail(bad), bad).toBe(false);
    }
  });
  // A non-breaking space is invisible in a chip and fatal at SMTP. `atext`
  // admits U+0080-U+FFFF wholesale, so the Unicode spaces have to be refused
  // by name — normalize_email in the store refuses them with the same rule.
  it('rejects invisible whitespace that would only fail at SMTP', () => {
    for (const bad of [
      'a b@example.com',
      'a\u00A0b@example.com',
      'a\u3000b@example.com',
      'ada@exam\u00A0ple.com',
      'ada\u00A0@example.com'
    ]) {
      expect(isValidEmail(bad), JSON.stringify(bad)).toBe(false);
    }
    // ...without costing the accented addresses the send edge does accept.
    expect(isValidEmail('jos\u00E9@example.com')).toBe(true);
  });
  // `lettre::Mailboxes` parses the whole header value — `mailbox_list` is
  // terminated by `eof`, so anything left over after the last mailbox fails the
  // parse. Pinned by `parse_mailboxes_rejects_text_left_over_after_the_angle_
  // address` in `crates/email/src/smtp.rs`.
  it('rejects syntax left over outside the angle address', () => {
    for (const bad of [
      'Ada <ada@example.com> trailing',
      '<ada@example.com> <bob@example.com>',
      '<ada@example.com>x',
      'ada@example.com>',
      'Ada <ada@example.com',
      'Ada <ada@example.com>, bob@example.com'
    ]) {
      expect(isValidEmail(bad), bad).toBe(false);
    }
    for (const good of ['Ada <ada@example.com>', '<ada@example.com>']) {
      expect(isValidEmail(good), good).toBe(true);
    }
  });
  // The send edge's size limits are BYTE limits: `email_address` compares
  // `str::len()` against 64/254/63. Counting UTF-16 code units read a 66-byte
  // accented local part as 33 and queued a draft that dies at SMTP. Pinned by
  // `parse_mailboxes_measures_addresses_in_utf8_bytes`.
  it('measures the size limits in UTF-8 bytes, not UTF-16 code units', () => {
    // 33 × 'é' is 66 bytes but only 33 code units.
    expect(isValidEmail(`${'é'.repeat(33)}@example.com`)).toBe(false);
    // 32 × 'é' is exactly 64 bytes, and the send edge accepts it.
    expect(isValidEmail(`${'é'.repeat(32)}@example.com`)).toBe(true);
    expect(isValidEmail(`${'a'.repeat(64)}@example.com`)).toBe(true);
    expect(isValidEmail(`ada@${'a'.repeat(63)}.com`)).toBe(true);
  });
  // `atext` on the Rust side is `char::is_alphanumeric` plus a fixed ASCII set,
  // NOT all of non-ASCII. Admitting U+0080-U+FFFF wholesale let a middle dot or
  // an emoji through the composer and into a send that fails.
  it('admits only the non-ASCII the send edge calls alphanumeric', () => {
    for (const bad of [
      'a·b@example.com',
      'a\u{1F600}b@example.com',
      'a–b@example.com',
      'ada@exam·ple.com'
    ]) {
      expect(isValidEmail(bad), JSON.stringify(bad)).toBe(false);
    }
    expect(isValidEmail('你好@example.com')).toBe(true);
  });
  // Deliberately NARROWER than the send edge, which parses a Unicode domain
  // fine and then hands it to a transport that refuses the envelope unless the
  // server advertises SMTPUTF8. A domain has a canonical ASCII spelling that
  // costs the recipient nothing, so the composer requires it. A local part has
  // no such spelling, which is why the accented ones above stay admitted.
  // Pinned by `a_unicode_domain_parses_here_and_the_composer_still_refuses_it`.
  it('requires an ASCII domain and accepts its punycode spelling', () => {
    for (const bad of ['ada@exämple.com', 'ada@例え.com', 'ada@example.中国']) {
      expect(isValidEmail(bad), JSON.stringify(bad)).toBe(false);
    }
    expect(isValidEmail('ada@xn--exmple-cua.com')).toBe(true);
    expect(isValidEmail('ada@xn--fiqs8s.example')).toBe(true);
  });
  it('keeps the addresses people actually use', () => {
    for (const good of [
      'ada@example.com',
      'ada.lovelace@example.co.uk',
      'me+filing@example.test',
      'a_b@example.test',
      'clerk-2@court.test',
      '1099@irs.test',
      'Ada Lovelace <Ada@Example.COM>'
    ]) {
      expect(isValidEmail(good), good).toBe(true);
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
  it('keeps a comma inside a quoted display name with its address', () => {
    expect(parseAddrs('"Doe, Jane" <j@x.io>, b@x.io')).toEqual([
      '"Doe, Jane" <j@x.io>',
      'b@x.io'
    ]);
  });
  it('does not split inside angle brackets', () => {
    expect(parseAddrs('Ada <ada@x.io>')).toEqual(['Ada <ada@x.io>']);
  });
  it('accepts a semicolon separator', () => {
    expect(parseAddrs('a@x.io; b@x.io')).toEqual(['a@x.io', 'b@x.io']);
  });
  // A display name with an odd number of literal quotes round-trips through
  // formatAddr as a quoted-pair escape. Toggling quote state on that escaped
  // quote leaves the parser inside quotes at the next comma, which swallows
  // every recipient after it into one broken chip.
  it('honours a quoted-pair escape and still splits the next recipient off', () => {
    const entry = formatAddr('bolt@vendor.io', '5" Bolt');
    expect(entry).toBe('"5\\" Bolt" <bolt@vendor.io>');
    expect(parseAddrs(`${entry}, b@x.io`)).toEqual([entry, 'b@x.io']);
    expect(addrLabel(entry)).toBe('5" Bolt');
    expect(isValidEmail(entry)).toBe(true);
  });
  it('keeps an escaped separator inside a display name', () => {
    const entry = formatAddr('jd@x.io', 'Doe, "JD", John; Esq.');
    expect(parseAddrs(`${entry}, second@x.io`)).toEqual([entry, 'second@x.io']);
    expect(addrLabel(entry)).toBe('Doe, "JD", John; Esq.');
  });
});

describe('serializeAddrs', () => {
  it('normalizes separators without changing the recipients', () => {
    expect(serializeAddrs('a@x.io,b@x.io')).toBe('a@x.io, b@x.io');
  });
  it('is idempotent, so a round trip never reads as an edit', () => {
    const once = serializeAddrs('  a@x.io ,  Ada <ada@x.io> ');
    expect(serializeAddrs(once)).toBe(once);
  });
});

describe('formatAddr', () => {
  it('pairs a plain name with its address', () => {
    expect(formatAddr('ada@x.io', 'Ada Lovelace')).toBe('Ada Lovelace <ada@x.io>');
  });
  it('drops a name that is just the address again', () => {
    expect(formatAddr('ada@x.io', 'ada@x.io')).toBe('ada@x.io');
    expect(formatAddr('ada@x.io', null)).toBe('ada@x.io');
  });
  it('quotes a name carrying RFC5322 specials so it survives a re-parse', () => {
    const entry = formatAddr('j@x.io', 'Doe, Jane');
    expect(entry).toBe('"Doe, Jane" <j@x.io>');
    expect(parseAddrs(entry)).toEqual([entry]);
    expect(isValidEmail(entry)).toBe(true);
  });
});

describe('addrLabel', () => {
  it('prefers the display name', () => {
    expect(addrLabel('Ada Lovelace <ada@x.io>')).toBe('Ada Lovelace');
    expect(addrLabel('"Doe, Jane" <j@x.io>')).toBe('Doe, Jane');
  });
  it('falls back to the address', () => {
    expect(addrLabel('ada@x.io')).toBe('ada@x.io');
    expect(addrLabel('<ada@x.io>')).toBe('ada@x.io');
  });
});

describe('addrKey', () => {
  it('folds case and display names to one identity', () => {
    expect(addrKey('Ada Lovelace <Ada@X.IO>')).toBe('ada@x.io');
    expect(addrKey('ada@x.io')).toBe(addrKey('ADA@X.IO'));
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
