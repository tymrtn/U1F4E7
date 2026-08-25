// Gmail-style search operators → IMAP search criteria.
//
// The per-account search endpoint passes the query to IMAP UID SEARCH (with a
// server-side TEXT fallback for bare terms). Operators the sweep found ignored
// (`from:yo@dev.to` matched other senders) are parsed client-side into real
// IMAP criteria; input that already reads as raw IMAP passes through untouched
// so CLI-style power queries keep working.
import { describe, expect, it } from 'vitest';
import { parseSearchQuery } from '$lib/search-query';

describe('parseSearchQuery', () => {
  it('maps from:/to:/subject: to quoted IMAP criteria', () => {
    expect(parseSearchQuery('from:yo@dev.to').imap).toBe('FROM "yo@dev.to"');
    expect(parseSearchQuery('to:dana@acme.com').imap).toBe('TO "dana@acme.com"');
    expect(parseSearchQuery('subject:invoice').imap).toBe('SUBJECT "invoice"');
  });

  it('supports quoted phrases in operators and free text', () => {
    expect(parseSearchQuery('subject:"quarterly report" budget').imap).toBe(
      'SUBJECT "quarterly report" TEXT "budget"'
    );
  });

  it('maps is:unread/is:read/is:starred and has:attachment-free equivalents', () => {
    expect(parseSearchQuery('is:unread').imap).toBe('UNSEEN');
    expect(parseSearchQuery('is:read').imap).toBe('SEEN');
    expect(parseSearchQuery('is:starred').imap).toBe('FLAGGED');
    expect(parseSearchQuery('is:flagged').imap).toBe('FLAGGED');
  });

  it('maps before:/after: dates to IMAP BEFORE/SINCE with RFC 3501 dates', () => {
    expect(parseSearchQuery('after:2026-08-01 before:2026-08-24').imap).toBe(
      'SINCE 1-Aug-2026 BEFORE 24-Aug-2026'
    );
  });

  it('combines operators with remaining free text as TEXT', () => {
    expect(parseSearchQuery('from:dana@acme.com is:unread pilot terms').imap).toBe(
      'FROM "dana@acme.com" UNSEEN TEXT "pilot terms"'
    );
  });

  it('escapes embedded quotes in values', () => {
    expect(parseSearchQuery('subject:"say \\"hi\\""').imap).toBe('SUBJECT "say \\"hi\\""');
  });

  it('passes through queries that already read as raw IMAP', () => {
    expect(parseSearchQuery('FROM boss@example.com UNSEEN').imap).toBe(
      'FROM boss@example.com UNSEEN'
    );
    expect(parseSearchQuery('SUBJECT invoice').imap).toBe('SUBJECT invoice');
    expect(parseSearchQuery('UNSEEN').imap).toBe('UNSEEN');
  });

  it('treats an unknown or malformed date as free text rather than guessing', () => {
    expect(parseSearchQuery('before:soon').imap).toBe('TEXT "before:soon"');
  });

  it('reports which operators it recognized for the UI chips', () => {
    const parsed = parseSearchQuery('from:dana@acme.com is:unread pilot');
    expect(parsed.operators).toEqual([
      { key: 'from', value: 'dana@acme.com' },
      { key: 'is', value: 'unread' }
    ]);
    expect(parsed.text).toBe('pilot');
  });
});
