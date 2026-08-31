import { describe, expect, it } from 'vitest';
import { initials } from './identity';

describe('initials', () => {
  it('takes first + last initial of a two-part name', () => {
    expect(initials('Maria Keller')).toBe('MK');
    expect(initials('J. Park')).toBe('JP');
  });

  it('uses first + last of a multi-part name', () => {
    expect(initials('Ana María Pérez Moreno')).toBe('AM');
  });

  it('strips a display-name/address pair to the name', () => {
    expect(initials('Maria Keller <maria@example.com>')).toBe('MK');
  });

  it('falls back to the local part for a bare address', () => {
    expect(initials('notifications@github.com')).toBe('NO');
    expect(initials('bowei.liu@imbue.example')).toBe('BL');
    expect(initials('newsletter@info.zoomadrid.com')).toBe('NE');
  });

  it('is unicode-safe on the first code point', () => {
    expect(initials('Émile Zola')).toBe('ÉZ');
    expect(initials('Ünïçøde')).toBe('ÜN');
  });

  it('returns a stable placeholder for empty input', () => {
    expect(initials('')).toBe('?');
    expect(initials('   ')).toBe('?');
  });

  it('handles a single-token name', () => {
    expect(initials('Apple')).toBe('AP');
  });
});
