// Tests for the shared compose drawer's recipient gating.
//
// Cc and Bcc are optional headers, but a malformed one still reaches real
// people via SMTP, so Send has to hold out for them the same way it does for
// To. Blank stays valid.

import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import ComposerDrawer from './ComposerDrawer.svelte';
import { getComposerStore, __resetComposerStore } from '$lib/composer.svelte';
import type { Account } from '$lib/api';

const ACCOUNTS: Account[] = [
  {
    id: 'acc1',
    name: 'Editor',
    username: 'editor@example.com',
    domain: 'example.com',
    smtp_host: 'smtp.example.com',
    smtp_port: 587,
    imap_host: 'imap.example.com',
    imap_port: 993
  }
];

/** Mount the drawer already open in compose mode with a valid To + Subject. */
async function renderCompose() {
  getComposerStore().open('compose', { accountId: 'acc1' });
  render(ComposerDrawer, { accounts: ACCOUNTS });
  await waitFor(() => expect(screen.getByLabelText('To')).toBeInTheDocument());
  await fireEvent.input(screen.getByLabelText('To'), { target: { value: 'buyer@example.com' } });
  await fireEvent.input(screen.getByLabelText('Subject'), { target: { value: 'Hello' } });
}

const sendButton = () => screen.getByRole('button', { name: /^send$/i });

beforeEach(() => {
  __resetComposerStore();
});

afterEach(() => {
  __resetComposerStore();
});

describe('ComposerDrawer recipient gating', () => {
  it('enables Send with a valid To and blank Cc/Bcc', async () => {
    await renderCompose();
    expect(sendButton()).toBeEnabled();
  });

  it('blocks Send when Cc is present but malformed', async () => {
    await renderCompose();
    await fireEvent.input(screen.getByLabelText('Cc'), { target: { value: 'broken' } });

    expect(sendButton()).toBeDisabled();
    expect(screen.getByText(/valid cc addresses/i)).toBeInTheDocument();
  });

  it('re-enables Send once a malformed Cc is corrected', async () => {
    await renderCompose();
    const cc = screen.getByLabelText('Cc');

    await fireEvent.input(cc, { target: { value: 'broken' } });
    expect(sendButton()).toBeDisabled();

    await fireEvent.input(cc, { target: { value: 'ops@example.com' } });
    expect(sendButton()).toBeEnabled();
  });

  it('blocks Send when one entry of a Cc list is malformed', async () => {
    await renderCompose();
    await fireEvent.input(screen.getByLabelText('Cc'), {
      target: { value: 'ops@example.com, broken' }
    });

    expect(sendButton()).toBeDisabled();
  });

  it('blocks Send when Bcc is present but malformed', async () => {
    await renderCompose();
    await fireEvent.click(screen.getByRole('button', { name: /^bcc$/i }));
    await fireEvent.input(screen.getByLabelText('Bcc'), { target: { value: 'nope@' } });

    expect(sendButton()).toBeDisabled();
    expect(screen.getByText(/valid bcc addresses/i)).toBeInTheDocument();
  });

  it('keeps Send enabled when Bcc is revealed but left blank', async () => {
    await renderCompose();
    await fireEvent.click(screen.getByRole('button', { name: /^bcc$/i }));

    expect(screen.getByLabelText('Bcc')).toBeInTheDocument();
    expect(sendButton()).toBeEnabled();
  });
});
