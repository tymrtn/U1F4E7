// Gmail-style search operators → IMAP SEARCH criteria.
//
// The dashboard's per-account search endpoint hands the query to IMAP
// UID SEARCH; the server wraps bare free text as TEXT "…" but passes anything
// that already starts with an IMAP key through untouched
// (crates/email/src/imap.rs::normalize_search_query). This module turns the
// operators people actually type (from:, to:, subject:, is:, before:, after:)
// into real criteria client-side, and leaves raw-IMAP power queries alone so
// CLI-style searches keep working.

export interface ParsedOperator {
  key: 'from' | 'to' | 'subject' | 'is' | 'before' | 'after';
  value: string;
}

export interface ParsedSearchQuery {
  /** The IMAP SEARCH criteria to send. */
  imap: string;
  /** Operators recognized, in input order — for chips/telemetry. */
  operators: ParsedOperator[];
  /** Free-text remainder (searched as TEXT). */
  text: string;
}

/** First tokens that mark a query as already-raw IMAP (kept in sync with the
 *  server's passthrough list conceptually; a conservative subset is enough —
 *  unknown-first-token queries just go through the operator parser). */
const RAW_IMAP_FIRST_TOKENS = new Set([
  'ALL', 'ANSWERED', 'BCC', 'BEFORE', 'BODY', 'CC', 'DELETED', 'DRAFT',
  'FLAGGED', 'FROM', 'HEADER', 'KEYWORD', 'LARGER', 'NEW', 'NOT', 'OLD', 'ON',
  'OR', 'RECENT', 'SEEN', 'SENTBEFORE', 'SENTON', 'SENTSINCE', 'SINCE',
  'SMALLER', 'SUBJECT', 'TEXT', 'TO', 'UID', 'UNANSWERED', 'UNDELETED',
  'UNDRAFT', 'UNFLAGGED', 'UNKEYWORD', 'UNSEEN'
]);

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

function quote(value: string): string {
  return `"${value.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

/** YYYY-MM-DD → RFC 3501 date (1-Aug-2026), or null when it isn't one. */
function rfc3501Date(value: string): string | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value);
  if (!m) return null;
  const [, y, mo, d] = m;
  const month = MONTHS[Number(mo) - 1];
  if (!month || Number(d) < 1 || Number(d) > 31) return null;
  return `${Number(d)}-${month}-${y}`;
}

const IS_FLAGS: Record<string, string> = {
  unread: 'UNSEEN',
  read: 'SEEN',
  starred: 'FLAGGED',
  flagged: 'FLAGGED',
  answered: 'ANSWERED',
  unanswered: 'UNANSWERED'
};

/** Tokenize honoring double quotes: `subject:"a b" c` → ['subject:"a b"', 'c']. */
function tokenize(input: string): string[] {
  const tokens: string[] = [];
  const re = /(?:[^\s"]+"(?:[^"\\]|\\.)*"|"(?:[^"\\]|\\.)*"|[^\s"]+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(input)) !== null) tokens.push(m[0]);
  return tokens;
}

function unquoteValue(raw: string): string {
  if (raw.startsWith('"') && raw.endsWith('"') && raw.length >= 2) {
    return raw.slice(1, -1).replace(/\\(.)/g, '$1');
  }
  return raw;
}

export function parseSearchQuery(input: string): ParsedSearchQuery {
  const trimmed = input.trim();
  if (!trimmed) return { imap: '', operators: [], text: '' };

  // Raw IMAP passes through untouched (parity with the server's rule).
  const firstToken = trimmed.split(/\s+/)[0].toUpperCase();
  if (trimmed.startsWith('(') || RAW_IMAP_FIRST_TOKENS.has(firstToken)) {
    return { imap: trimmed, operators: [], text: '' };
  }

  const criteria: string[] = [];
  const operators: ParsedOperator[] = [];
  const freeText: string[] = [];

  for (const token of tokenize(trimmed)) {
    const opMatch = /^(from|to|subject|is|before|after):(.+)$/i.exec(token);
    if (!opMatch) {
      freeText.push(unquoteValue(token));
      continue;
    }
    const key = opMatch[1].toLowerCase() as ParsedOperator['key'];
    const value = unquoteValue(opMatch[2]);
    if (key === 'from' || key === 'to' || key === 'subject') {
      criteria.push(`${key.toUpperCase()} ${quote(value)}`);
      operators.push({ key, value });
    } else if (key === 'is') {
      const flag = IS_FLAGS[value.toLowerCase()];
      if (flag) {
        criteria.push(flag);
        operators.push({ key, value: value.toLowerCase() });
      } else {
        freeText.push(token);
      }
    } else {
      const date = rfc3501Date(value);
      if (date) {
        criteria.push(`${key === 'before' ? 'BEFORE' : 'SINCE'} ${date}`);
        operators.push({ key, value });
      } else {
        // A date we can't read is kept as literal text rather than guessed at.
        freeText.push(token);
      }
    }
  }

  const text = freeText.join(' ');
  if (text) criteria.push(`TEXT ${quote(text)}`);
  return { imap: criteria.join(' '), operators, text };
}
