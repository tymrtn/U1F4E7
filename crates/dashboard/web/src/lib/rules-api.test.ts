// Unit tests for the rules editor's match-expr build/parse round-trip,
// focused on `from_exact` (literal sender match) vs `from` (glob sender
// match) — the two must never be silently conflated by the editor.

import { describe, expect, it } from 'vitest';
import { buildMatchExpr, parseMatchExpr, matchSummary, type MatchFields } from './rules-api';

describe('from_exact match-expr build/parse round-trip', () => {
  it('buildMatchExpr emits from_exact (not from) when fromExact is set', () => {
    const expr = buildMatchExpr({ fromExact: 'alice@example.com' }) as Record<string, unknown>;
    expect(expr).toEqual({ from_exact: 'alice@example.com' });
    expect(expr.from).toBeUndefined();
  });

  it('buildMatchExpr still emits from (glob) when only from is set', () => {
    const expr = buildMatchExpr({ from: '*@example.com' }) as Record<string, unknown>;
    expect(expr).toEqual({ from: '*@example.com' });
  });

  it('buildMatchExpr prefers fromExact over from when both are somehow set', () => {
    const expr = buildMatchExpr({ from: '*@example.com', fromExact: 'alice@example.com' }) as Record<
      string,
      unknown
    >;
    expect(expr).toEqual({ from_exact: 'alice@example.com' });
  });

  it('parseMatchExpr reads a stored {from_exact} rule into fields.fromExact, not fields.from', () => {
    const fields = parseMatchExpr('{"from_exact":"alice@example.com"}');
    expect(fields.fromExact).toBe('alice@example.com');
    expect(fields.from).toBeUndefined();
  });

  it('parseMatchExpr reads a stored {from} (glob) rule into fields.from, not fields.fromExact', () => {
    const fields = parseMatchExpr('{"from":"*@example.com"}');
    expect(fields.from).toBe('*@example.com');
    expect(fields.fromExact).toBeUndefined();
  });

  it('round-trips {from_exact} through build -> parse -> build without ever becoming a glob', () => {
    const original = '{"from_exact":"*@example.com"}'; // wildcard local-part — must stay literal
    const fields = parseMatchExpr(original);
    expect(fields.fromExact).toBe('*@example.com');
    const rebuilt = buildMatchExpr(fields as MatchFields);
    expect(rebuilt).toEqual({ from_exact: '*@example.com' });
    expect(JSON.stringify(rebuilt)).not.toBe('{"from":"*@example.com"}');
  });

  it('round-trips an AND of {from_exact} + {subject} unchanged', () => {
    const original = '{"and":[{"from_exact":"alice@example.com"},{"subject":"*invoice*"}]}';
    const fields = parseMatchExpr(original);
    expect(fields).toMatchObject({ fromExact: 'alice@example.com', subject: '*invoice*' });
    const rebuilt = buildMatchExpr(fields as MatchFields);
    expect(rebuilt).toEqual({
      and: [{ from_exact: 'alice@example.com' }, { subject: '*invoice*' }]
    });
  });

  it('matchSummary describes from_exact distinctly from a glob from', () => {
    expect(matchSummary('{"from_exact":"alice@example.com"}')).toBe('from exactly alice@example.com');
    expect(matchSummary('{"from":"*@example.com"}')).toBe('from *@example.com');
  });
});
