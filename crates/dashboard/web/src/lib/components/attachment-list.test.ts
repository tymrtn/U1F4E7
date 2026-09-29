// AttachmentList: a blocked attachment (rShield download gate) is shown as
// blocked with the reason and no download affordance. The download route
// refuses it, so a link would only start a download that fails silently.

import { render, screen, within } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';

import AttachmentList from './AttachmentList.svelte';

const ATTACHMENTS = [
  { filename: 'notes.pdf', content_type: 'application/pdf', size: 2048 },
  { filename: 'invoice.pdf.exe', content_type: 'application/octet-stream', size: 512 }
];

describe('AttachmentList', () => {
  it('offers a download link for an attachment the gate allows', () => {
    render(AttachmentList, { attachments: ATTACHMENTS, accountId: 'a1', uid: 7, blocks: [] });
    const link = screen.getByRole('link', { name: 'Download invoice.pdf.exe' });
    expect(link).toHaveAttribute('download', 'invoice.pdf.exe');
  });

  it('renders a blocked attachment with its reason and no download link', () => {
    render(AttachmentList, {
      attachments: ATTACHMENTS,
      accountId: 'a1',
      uid: 7,
      blocks: [
        {
          filename: 'invoice.pdf.exe',
          code: 'attachment_blocked',
          reason: 'attachment looks like malware (double_extension)'
        }
      ]
    });

    expect(screen.getByRole('link', { name: 'Download notes.pdf' })).toBeInTheDocument();
    expect(screen.queryByRole('link', { name: /invoice\.pdf\.exe/ })).not.toBeInTheDocument();

    const list = document.getElementById('attachment-list') as HTMLElement;
    const card = within(list).getByText('invoice.pdf.exe').closest('.attachment-card') as HTMLElement;
    expect(card.tagName).not.toBe('A');
    expect(card).toHaveTextContent(/Blocked/);
    expect(card).toHaveTextContent('attachment looks like malware (double_extension)');
    expect(card.querySelector('[href]')).toBeNull();
  });
});
