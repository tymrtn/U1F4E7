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

/**
 * The exact return time a person will see, in their local zone and named, so a
 * snooze set before a DST change still reads correctly after it (`Sun, Nov 1,
 * 8:00 AM EST`). `timeZone` is only for tests; the UI uses the viewer's zone.
 */
export function formatExactReturn(at: Date, timeZone?: string): string {
  return at.toLocaleString(undefined, {
    weekday: 'short',
    month: 'short',
    day: 'numeric',
    year: at.getFullYear() === new Date().getFullYear() ? undefined : 'numeric',
    hour: 'numeric',
    minute: '2-digit',
    timeZoneName: 'short',
    timeZone
  });
}

/**
 * Parse an `<input type="datetime-local">` value (`YYYY-MM-DDTHH:MM`) as local
 * wall-clock time. Returns null for anything unparseable or not in the future
 * of `now`, so a custom snooze can never be sent for a past instant.
 */
export function parseCustomSnooze(value: string, now: Date): Date | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})$/.exec(value.trim());
  if (!m) return null;
  const [, y, mo, d, h, mi] = m.map(Number);
  const at = new Date(y, mo - 1, d, h, mi, 0, 0);
  // Reject values that do not exist locally (e.g. 02:30 on a spring-forward
  // day), which Date silently shifts to another hour.
  if (at.getHours() !== h || at.getMinutes() !== mi || at.getDate() !== d) return null;
  return at.getTime() > now.getTime() ? at : null;
}
