// ThreatBanner — the reader's verdict card: level wording, "Why?" arithmetic,
// Mark safe, and Report (draft only).
import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const readerApiMock = vi.hoisted(() => ({
  postThreatMarkSafe: vi.fn(),
  postThreatReport: vi.fn()
}));

vi.mock('$lib/reader-api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/reader-api')>();
  return {
    ...actual,
    postThreatMarkSafe: readerApiMock.postThreatMarkSafe,
    postThreatReport: readerApiMock.postThreatReport
  };
});

import ThreatBanner from './ThreatBanner.svelte';
import type { ThreatView } from '$lib/reader-api';

const dangerous: ThreatView = {
  level: 'dangerous',
  score: 100,
  malware: true,
  marked_safe: false,
  signals: [{ code: 'lookalike_domain', weight: 45, evidence: 'sender domain examp1e.org imitates example.org' }],
  explain: ['+ 45  lookalike_domain  (sender domain examp1e.org imitates example.org)', '= 115, capped at 100', 'level dangerous (score >= 70)']
};

const props = { accountId: 'acc1', uid: 7, folder: 'INBOX' };

beforeEach(() => {
  readerApiMock.postThreatMarkSafe.mockReset();
  readerApiMock.postThreatReport.mockReset();
});

describe('ThreatBanner', () => {
  it('renders nothing for clean mail', () => {
    const { container } = render(ThreatBanner, { threat: { level: 'clean', score: 5 }, ...props });
    expect(container.querySelector('#threat-banner')).toBeNull();
  });

  it('shows the dangerous level, score, blocked attachments, and the arithmetic on Why?', async () => {
    render(ThreatBanner, { threat: dangerous, ...props });
    expect(screen.getByRole('alert').textContent).toContain('This message looks dangerous');
    expect(screen.getByText('100/100')).toBeTruthy();
    expect(screen.getByText(/attachments are blocked/i)).toBeTruthy();
    expect(screen.queryByText('= 115, capped at 100')).toBeNull();
    await fireEvent.click(screen.getByRole('button', { name: 'Why?' }));
    expect(screen.getByText('= 115, capped at 100')).toBeTruthy();
  });

  it('Mark safe posts and hands the new verdict up', async () => {
    const onchange = vi.fn();
    readerApiMock.postThreatMarkSafe.mockResolvedValue({
      status: 'marked_safe',
      threat: { ...dangerous, marked_safe: true }
    });
    render(ThreatBanner, { threat: dangerous, ...props, onchange });
    await fireEvent.click(screen.getByRole('button', { name: 'Mark safe' }));
    await vi.waitFor(() => expect(onchange).toHaveBeenCalled());
    expect(readerApiMock.postThreatMarkSafe).toHaveBeenCalledWith('acc1', 7, 'INBOX');
    expect(onchange.mock.calls[0][0].marked_safe).toBe(true);
  });

  it('Report creates a draft and says it was not sent', async () => {
    readerApiMock.postThreatReport.mockResolvedValue({
      status: 'drafted',
      sent: false,
      draft_id: 'd-1',
      to: 'reportphishing@apwg.org',
      subject: 'Phishing report: x',
      imap_folder: 'Drafts'
    });
    render(ThreatBanner, { threat: dangerous, ...props });
    await fireEvent.click(screen.getByRole('button', { name: 'Report' }));
    const link = await screen.findByText(/Report drafted, not sent/);
    expect(link.getAttribute('href')).toContain('/accounts/acc1/drafts/d-1');
  });

  it('an unavailable verdict never reads as clean and offers no Mark safe', () => {
    render(ThreatBanner, { threat: { level: 'unavailable', error: 'ledger unreadable' }, ...props });
    expect(screen.getByRole('status').textContent).toContain('could not finish');
    expect(screen.queryByRole('button', { name: 'Mark safe' })).toBeNull();
  });
});
