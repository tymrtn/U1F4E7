import { afterEach, describe, expect, it } from 'vitest';
// Vitest runs in Node; switching TZ at runtime is how DST is exercised here.
declare const process: { env: Record<string, string | undefined> };

import { formatExactReturn, parseCustomSnooze, snoozeOptions } from './snooze-options';

describe('snoozeOptions', () => {
  it('offers Later today in the morning, at 5pm the same day', () => {
    const now = new Date(2026, 7, 28, 9, 15, 0); // Fri 28 Aug 2026, 09:15
    const opts = snoozeOptions(now);
    const later = opts.find((o) => o.key === 'later-today');
    expect(later).toBeTruthy();
    expect(later!.at.getDate()).toBe(28);
    expect(later!.at.getHours()).toBe(17);
    expect(later!.at.getMinutes()).toBe(0);
  });

  it('drops Later today once it is past the cutoff', () => {
    const now = new Date(2026, 7, 28, 18, 0, 0); // 6pm — too late
    const opts = snoozeOptions(now);
    expect(opts.find((o) => o.key === 'later-today')).toBeUndefined();
  });

  it('never offers a time in the past', () => {
    const now = new Date(2026, 7, 28, 14, 0, 0);
    for (const o of snoozeOptions(now)) {
      expect(o.at.getTime()).toBeGreaterThan(now.getTime());
    }
  });

  it('sets Tomorrow to 8am the next day', () => {
    const now = new Date(2026, 7, 28, 9, 0, 0); // Fri
    const tom = snoozeOptions(now).find((o) => o.key === 'tomorrow')!;
    expect(tom.at.getDate()).toBe(29);
    expect(tom.at.getHours()).toBe(8);
  });

  it('sets This weekend to the coming Saturday', () => {
    const now = new Date(2026, 7, 26, 9, 0, 0); // Wed 26 Aug 2026
    const wk = snoozeOptions(now).find((o) => o.key === 'weekend')!;
    expect(wk.at.getDay()).toBe(6); // Saturday
    expect(wk.at.getDate()).toBe(29);
  });

  it('sets Next week to the coming Monday, never today', () => {
    const monday = new Date(2026, 7, 31, 9, 0, 0); // Mon 31 Aug 2026
    const nw = snoozeOptions(monday).find((o) => o.key === 'next-week')!;
    expect(nw.at.getDay()).toBe(1);
    expect(nw.at.getDate()).toBe(7); // next Mon, not today
  });

  it('produces Dates the caller can send as a UTC instant', () => {
    // The endpoint's sweep compares against UTC now, so the row sends
    // opt.at.toISOString(); every option must round-trip to a valid instant.
    for (const o of snoozeOptions(new Date(2026, 7, 28, 9, 0, 0))) {
      expect(o.at.toISOString()).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/);
    }
  });
});

describe('snooze across DST and time zones (#170)', () => {
  const savedTz = process.env.TZ;
  afterEach(() => {
    process.env.TZ = savedTz;
  });

  it('Tomorrow stays 8:00 local across the US fall-back, and the UTC instant shifts by the hour', () => {
    process.env.TZ = 'America/New_York';
    // Sat 31 Oct 2026 10:00 EDT; DST ends Sun 1 Nov 02:00.
    const now = new Date(2026, 9, 31, 10, 0, 0);
    const tom = snoozeOptions(now).find((o) => o.key === 'tomorrow')!;
    expect(tom.at.getHours()).toBe(8);
    expect(tom.at.toISOString()).toBe('2026-11-01T13:00:00.000Z'); // 08:00 EST = 13:00Z
    // Round-trip through the wire format lands on the same instant.
    expect(new Date(tom.at.toISOString()).getTime()).toBe(tom.at.getTime());
  });

  it('Next week stays 8:00 local across the EU spring-forward', () => {
    process.env.TZ = 'Europe/Madrid';
    // Thu 26 Mar 2026 12:00 CET; DST starts Sun 29 Mar.
    const now = new Date(2026, 2, 26, 12, 0, 0);
    const nw = snoozeOptions(now).find((o) => o.key === 'next-week')!;
    expect(nw.at.getDay()).toBe(1);
    expect(nw.at.getHours()).toBe(8);
    expect(nw.at.toISOString()).toBe('2026-03-30T06:00:00.000Z'); // 08:00 CEST = 06:00Z
  });

  it('shows the exact return in the viewer’s zone, naming the zone on each side of DST', () => {
    const before = formatExactReturn(new Date('2026-10-31T12:00:00Z'), 'America/New_York');
    const after = formatExactReturn(new Date('2026-11-01T13:00:00Z'), 'America/New_York');
    expect(before).toMatch(/8:00/);
    expect(before).toMatch(/EDT/);
    expect(after).toMatch(/8:00/);
    expect(after).toMatch(/EST/);
    // The same stored instant reads differently in another zone.
    expect(formatExactReturn(new Date('2026-11-01T13:00:00Z'), 'Asia/Kolkata')).toMatch(/6:30/);
  });

  it('parses a custom local time, and refuses past or nonexistent local times', () => {
    process.env.TZ = 'America/New_York';
    const now = new Date(2026, 2, 7, 12, 0, 0);
    const ok = parseCustomSnooze('2026-03-09T09:30', now)!;
    expect(ok.getHours()).toBe(9);
    expect(ok.getMinutes()).toBe(30);
    expect(parseCustomSnooze('2026-03-01T09:30', now)).toBeNull(); // past
    expect(parseCustomSnooze('2026-03-08T02:30', now)).toBeNull(); // skipped by spring-forward
    expect(parseCustomSnooze('tomorrow', now)).toBeNull();
  });
});
