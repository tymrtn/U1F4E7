// The reader column's empty state must describe what opening does. Tyler
// 2026-09-29: opening a message marks it read.
import { render, screen } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import MailEmpty from '../../routes/mail/[box]/+page.svelte';

describe('mail reader empty state', () => {
  it('says opening marks the message read, never that it stays unread', () => {
    render(MailEmpty);
    expect(screen.getByText('Open a message to read it')).toBeInTheDocument();
    expect(screen.getByText(/Opening a message marks it read\./)).toBeInTheDocument();
    expect(document.body.textContent).not.toMatch(/leaves it unread/);
  });
});
