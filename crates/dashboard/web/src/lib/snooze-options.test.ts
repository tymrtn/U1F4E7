import { describe, expect, it } from 'vitest';
import { snoozeOptions } from './snooze-options';

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
