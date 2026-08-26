// Recipient-address parsing and validation, shared by every surface that can
// put mail on the wire.
//
// The composer drawer and the draft review composer have to agree on what
// counts as a usable recipient. If they drift, a draft that Compose would
// refuse to send can still be queued from the review page, and the empty
// address only surfaces at SMTP time.

/**
 * The text inside the entry's angle address, or `null` when there isn't one.
 *
 * The bracket pair has to be the only one outside quotes AND the `>` has to be
 * the last character of the entry. `lettre::Mailboxes` parses the whole header
 * value — its `mailbox_list` is terminated by `eof` — so text left over after
 * the last mailbox fails the parse rather than being ignored. Matching the
 * brackets with a substring search instead found `<ada@example.com>` inside
 * `Ada <ada@example.com> trailing` and validated only that.
 *
 * Quote state is tracked the way `parseAddrs` tracks it, so a display name may
 * carry a bracket (`"a<b" <x@y.io>`) without the entry reading as malformed.
 */
function angleAddress(entry: string): string | null {
  let inQuotes = false;
  let escaped = false;
  let open = -1;

  for (let i = 0; i < entry.length; i++) {
    const ch = entry[i];
    if (escaped) {
      escaped = false;
    } else if (ch === '\\' && inQuotes) {
      escaped = true;
    } else if (ch === '"') {
      inQuotes = !inQuotes;
    } else if (!inQuotes) {
      // Brackets inside a display name are text, not structure.
      if (ch === '<') {
        if (open >= 0) return null;
        open = i;
      } else if (ch === '>') {
        if (open < 0 || i !== entry.length - 1) return null;
        return entry.slice(open + 1, i);
      }
    }
  }
  return null;
}

/**
 * Strip a display name, leaving the bare address: `Ada <a@x.io>` → `a@x.io`.
 *
 * An entry whose brackets do not form one well-formed angle address comes back
 * whole, stray syntax included, so `isValidEmail` sees the leftover text and
 * refuses the entry instead of validating a fragment mined out of it.
 */
export function normalizedAddress(addr: string): string {
  const entry = addr.trim();
  return angleAddress(entry)?.trim() ?? entry;
}

// RFC 5322 `atext`, the character set a dot-atom local part is built from.
// `email_address::is_atext` — the check `lettre::Address` runs on the local part
// — is `char::is_alphanumeric()`, meaning the Alphabetic property or general
// category Nd, Nl or No, plus a fixed ASCII set. Admitting U+0080-U+FFFF
// wholesale, as this used to, let a middle dot and an emoji past the composer
// and into a send that fails with InvalidUser.
//
// The Rust side has one more arm, `is_utf8_non_ascii`, which its byte-pattern
// match only ever reaches for unassigned code points that are not alphanumeric.
// Refusing those is a narrowing, not a divergence: nothing sendable is lost.
const ALNUM = '\\p{Alphabetic}\\p{Nd}\\p{Nl}\\p{No}';
const ATEXT = ALNUM + "!#$%&'*+/=?^_`{|}~-";
const DOT_ATOM = new RegExp(`^[${ATEXT}]+(?:\\.[${ATEXT}]+)*$`, 'u');

// Domain labels are ASCII letter-digit-hyphen, hyphens never on a boundary.
// Narrower than the send edge, deliberately — see `isValidEmail`.
const DOMAIN_LABEL = '[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?';
const DOMAIN = new RegExp(`^${DOMAIN_LABEL}(?:\\.${DOMAIN_LABEL})+$`);

// A local part must be a dot-atom. RFC 5322 also allows a quoted string
// (`"john..doe"@example.com`), and `lettre::Address::from_str` accepts one —
// but recipient headers do not go through that. Every To/Cc/Bcc value is
// parsed by `lettre::Mailboxes`, which refuses a quoted local part outright,
// so accepting one here would queue a draft that fails at SMTP with
// InvalidUser. Pinned by `parse_mailboxes_rejects_what_the_suggestion_and_
// composer_gates_also_reject` in `crates/email/src/smtp.rs`.

// Whitespace or a control character anywhere in the candidate. The shapes above
// exclude these too, but a non-breaking space is invisible in a chip and fatal
// at SMTP, so the rule is spelled out rather than left to fall out of `atext` —
// `normalize_email` in `crates/store/src/address_book.rs` spells out the same
// one, so the composer and the address book are legibly the same gate.
const UNPRINTABLE = /[\s\u0000-\u001F\u007F]/;

// RFC 5321 §4.5.3.1 size limits, which the send path enforces — as BYTE limits.
// `email_address` compares `str::len()` against each of these, so measuring in
// UTF-16 code units read a 66-byte accented local part as 33 and queued a draft
// that only failed at SMTP. Pinned by
// `parse_mailboxes_measures_addresses_in_utf8_bytes` in `crates/email/src/smtp.rs`.
const MAX_LOCAL_PART = 64;
const MAX_DOMAIN = 254;
const MAX_LABEL = 63;

const utf8 = new TextEncoder();
const byteLength = (part: string): number => utf8.encode(part).length;

/**
 * True when this entry names an address the send path will actually accept.
 *
 * Deliberately not a loose `something@something.something`. `lettre::Mailboxes`
 * — which every To/Cc/Bcc value parses through — rejects consecutive or edge
 * dots in the local part, domain labels that do not start and end alphanumeric,
 * quoted local parts, and Unicode whitespace. A looser rule here would let the
 * composer queue a draft that only fails at SMTP time.
 *
 * The split on the LAST `@` matches lettre's own `rsplitn(2, '@')`.
 * `normalize_email` in `crates/store/src/address_book.rs` admits the same set
 * from the suggestion side, so the dropdown can never offer an address this
 * refuses. All three are pinned against the send edge by
 * `parse_mailboxes_rejects_what_the_suggestion_and_composer_gates_also_reject`.
 *
 * On one axis this is NARROWER than the send edge, on purpose: the domain has
 * to be ASCII. `Address::check_domain` parses a Unicode domain — either `atext`
 * takes it or the `idna` fallback does — but nothing rewrites it, so the
 * address goes on the wire in its Unicode spelling and `Connection::send` then
 * refuses the whole envelope unless the server advertises SMTPUTF8. A domain
 * has a canonical ASCII spelling (punycode) that costs the recipient nothing,
 * so requiring it here trades no real address for a delivery that no longer
 * depends on an extension the server may not have. A local part has no such
 * spelling, which is why accented ones stay admitted on every layer.
 */
export function isValidEmail(addr: string): boolean {
  const candidate = normalizedAddress(addr);
  if (UNPRINTABLE.test(candidate)) return false;

  const at = candidate.lastIndexOf('@');
  if (at <= 0) return false;

  const local = candidate.slice(0, at);
  const domain = candidate.slice(at + 1);
  return (
    byteLength(local) <= MAX_LOCAL_PART &&
    byteLength(domain) <= MAX_DOMAIN &&
    DOT_ATOM.test(local) &&
    DOMAIN.test(domain) &&
    domain.split('.').every((label) => byteLength(label) <= MAX_LABEL)
  );
}

/**
 * Split a comma-separated header value into its non-empty entries.
 *
 * Separators inside a quoted display name or an angle-bracketed address are
 * part of the entry, not a split point: `"Doe, Jane" <j@x.io>` is ONE
 * recipient. Splitting it naively would both invalidate a legitimate address
 * and let the recipient field render it as two broken chips.
 *
 * Quoted-pair escapes are honoured, because `formatAddr` emits them: a display
 * name carrying an odd number of literal quotes (`5" Bolt`) comes back as
 * `"5\" Bolt"`, and toggling quote state on that escaped quote would leave the
 * parser inside quotes at the next comma — swallowing the recipient after it.
 * Mirrors `split_address_list` in `crates/store/src/address_book.rs`.
 */
export function parseAddrs(raw: string): string[] {
  const entries: string[] = [];
  let current = '';
  let inQuotes = false;
  let inAngles = false;
  let escaped = false;

  for (const ch of raw) {
    if (escaped) {
      current += ch;
      escaped = false;
      continue;
    }
    if (ch === '\\' && inQuotes) {
      current += ch;
      escaped = true;
      continue;
    }
    if (ch === '"') {
      inQuotes = !inQuotes;
    } else if (ch === '<' && !inQuotes) {
      inAngles = true;
    } else if (ch === '>' && !inQuotes) {
      inAngles = false;
    } else if ((ch === ',' || ch === ';') && !inQuotes && !inAngles) {
      entries.push(current);
      current = '';
      continue;
    }
    current += ch;
  }
  entries.push(current);

  return entries.map((entry) => entry.trim()).filter(Boolean);
}

/**
 * Canonical header form for a list of entries.
 *
 * The recipient field round-trips its bound value through this, so a draft
 * loaded from the server has to be normalized the same way before it is taken
 * as an editing baseline — otherwise re-serialization alone reads as an unsaved
 * change.
 */
export function serializeAddrs(raw: string): string {
  return parseAddrs(raw).join(', ');
}

/**
 * Build one header entry from an address and an optional display name.
 *
 * Names carrying RFC5322 specials are quoted so the entry survives
 * `parseAddrs` unchanged; a name that is just the address again adds nothing
 * and is dropped.
 */
export function formatAddr(email: string, name?: string | null): string {
  const display = (name ?? '').trim();
  if (!display || display.toLowerCase() === email.toLowerCase()) return email;
  const quoted = /["(),:;<>@[\]\\]/.test(display)
    ? `"${display.replace(/([\\"])/g, '\\$1')}"`
    : display;
  return `${quoted} <${email}>`;
}

/** Human label for a recipient chip: the display name when there is one. */
export function addrLabel(entry: string): string {
  const angle = entry.indexOf('<');
  if (angle > 0) {
    const name = entry.slice(0, angle).trim().replace(/^"|"$/g, '').replace(/\\(["\\])/g, '$1');
    if (name) return name;
  }
  return normalizedAddress(entry);
}

/** Case-folded address used as the identity of a recipient across fields. */
export function addrKey(entry: string): string {
  return normalizedAddress(entry).toLowerCase();
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
