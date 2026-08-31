// Snooze-time presets for the row verb cluster. The user always picks an
// explicit, labeled time — no hidden default. Each option is a concrete local
// `Date`; the caller sends it as a UTC instant (`.toISOString()`), matching
// BulkToolbar. The snooze endpoint's unsnooze sweep compares against UTC now,
// so a naive local string would fire off by the user's UTC offset.

export interface SnoozeOption {
  key: string;
  label: string;
  /** Concrete return time; also rendered to the user as a hint. */
  at: Date;
}

const LATER_TODAY_HOUR = 17; // 5pm
const MORNING_HOUR = 8; // 8am

function atHour(base: Date, addDays: number, hour: number): Date {
  const d = new Date(base);
  d.setDate(d.getDate() + addDays);
  d.setHours(hour, 0, 0, 0);
  return d;
}

/** Days until the next given weekday (0=Sun … 6=Sat); 7 if today is it. */
function daysUntilWeekday(from: Date, weekday: number): number {
  const delta = (weekday - from.getDay() + 7) % 7;
  return delta === 0 ? 7 : delta;
}

/**
 * Preset snooze options relative to `now`. "Later today" is dropped once it is
 * past the cutoff, so the menu never offers a time in the past.
 */
export function snoozeOptions(now: Date): SnoozeOption[] {
  const options: SnoozeOption[] = [];

  if (now.getHours() < LATER_TODAY_HOUR - 1) {
    options.push({ key: 'later-today', label: 'Later today', at: atHour(now, 0, LATER_TODAY_HOUR) });
  }
  options.push({ key: 'tomorrow', label: 'Tomorrow', at: atHour(now, 1, MORNING_HOUR) });
  options.push({
    key: 'weekend',
    label: 'This weekend',
    at: atHour(now, daysUntilWeekday(now, 6), MORNING_HOUR)
  });
  options.push({
    key: 'next-week',
    label: 'Next week',
    at: atHour(now, daysUntilWeekday(now, 1), MORNING_HOUR)
  });

  return options;
}
