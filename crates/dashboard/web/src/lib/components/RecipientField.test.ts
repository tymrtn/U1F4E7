// Tests for the shared recipient token field.
//
// This control sits between the operator and every outbound message, so the
// cases here are the ones where getting it wrong puts mail on the wire with
// the wrong recipients — or refuses to put it on the wire at all:
//
//   • an address typed but never "committed" must still reach `value`
//   • an address autocomplete has never seen must be addable
//   • a stale suggestion response must not repaint a newer prefix
//   • the same person must not land in both To and Cc

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import Harness from './RecipientField.harness.svelte';
import { createSuggester, resetSuggestionCache } from '$lib/recipient-suggestions';
import type { Suggester } from '$lib/recipient-suggestions';
import type { AddressSuggestion } from '$lib/api';

const ADA: AddressSuggestion = { email: 'ada@example.test', name: 'Ada Lovelace' };
const ADAM: AddressSuggestion = { email: 'adam@example.test', name: null };

/** A suggester backed by a fixed row set, with no cache and no network. */
function stubSuggester(rows: AddressSuggestion[] = [ADA, ADAM]): Suggester {
  return {
    cached: () => undefined,
    search: async () => rows,
    cancel: () => {}
  };
}

/**
 * A suggester whose responses are resolved by hand, in test-chosen order, with
 * an optional pre-seeded cache. Mirrors the real module's sequencing: a search
 * superseded by a newer one — or by `cancel()` — resolves to null.
 */
function manualSuggester(cache: Record<string, AddressSuggestion[]> = {}) {
  const pending: { query: string; resolve: (rows: AddressSuggestion[] | null) => void }[] = [];
  let latest = 0;

  const suggester: Suggester = {
    cached: (_account, query) => cache[query],
    search: (_account, query) => {
      const id = ++latest;
      return new Promise((resolve) => {
        pending.push({ query, resolve: (rows) => resolve(id === latest ? rows : null) });
      });
    },
    cancel: () => {
      latest += 1;
    }
  };

  return { suggester, pending };
}

/** Latest header string the field published to its parent. */
let latestValue = '';

beforeEach(() => {
  latestValue = '';
});

/** Render with suggestions available. */
function setup(props: Record<string, unknown> = {}) {
  return render(Harness, {
    props: {
      suggester: stubSuggester(),
      onvalue: (value: string) => (latestValue = value),
      ...props
    }
  });
}

/** Render with an empty address book, so Enter is never armed by a dropdown. */
function setupWithoutSuggestions(props: Record<string, unknown> = {}) {
  return setup({ suggester: stubSuggester([]), ...props });
}

/** What the field has published to its parent. */
function published(): string {
  return latestValue;
}

function toInput(): HTMLInputElement {
  return screen.getByRole('combobox') as HTMLInputElement;
}

async function type(text: string) {
  await fireEvent.input(toInput(), { target: { value: text } });
}

async function paste(text: string) {
  await fireEvent.paste(toInput(), { clipboardData: { getData: () => text } });
}

describe('RecipientField — suggestions', () => {
  it('opens a listbox of suggestions while typing', async () => {
    setup();
    await type('ad');

    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(2));
    expect(screen.getByText('Ada Lovelace')).toBeInTheDocument();
    expect(toInput()).toHaveAttribute('aria-expanded', 'true');
  });

  it('does not query until something is typed', async () => {
    const search = vi.fn().mockResolvedValue([ADA]);
    setup({ suggester: { cached: () => undefined, search, cancel: () => {} } });

    expect(search).not.toHaveBeenCalled();
    await type('   ');
    expect(search).not.toHaveBeenCalled();
  });

  it('selects the highlighted option with Enter', async () => {
    setup();
    await type('ad');
    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(2));

    await fireEvent.keyDown(toInput(), { key: 'Enter' });

    expect(published()).toBe('Ada Lovelace <ada@example.test>');
    expect(toInput()).toHaveValue('');
    expect(screen.queryAllByRole('option')).toHaveLength(0);
  });

  it('moves the active option with ArrowDown/ArrowUp and wraps at both ends', async () => {
    setup();
    await type('ad');
    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(2));

    const options = () => screen.getAllByRole('option');
    expect(options()[0]).toHaveAttribute('aria-selected', 'true');

    await fireEvent.keyDown(toInput(), { key: 'ArrowDown' });
    expect(options()[1]).toHaveAttribute('aria-selected', 'true');
    expect(toInput()).toHaveAttribute('aria-activedescendant', 'field-to-listbox-option-1');

    await fireEvent.keyDown(toInput(), { key: 'ArrowDown' });
    expect(options()[0]).toHaveAttribute('aria-selected', 'true');
    await fireEvent.keyDown(toInput(), { key: 'ArrowUp' });
    expect(options()[1]).toHaveAttribute('aria-selected', 'true');

    await fireEvent.keyDown(toInput(), { key: 'Enter' });
    expect(published()).toBe('adam@example.test');
  });

  it('selects with Tab', async () => {
    setup();
    await type('ad');
    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(2));

    await fireEvent.keyDown(toInput(), { key: 'Tab' });
    expect(published()).toBe('Ada Lovelace <ada@example.test>');
  });

  it('selects with the mouse', async () => {
    setup();
    await type('ad');
    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(2));

    await fireEvent.mouseDown(screen.getAllByRole('option')[1]);
    expect(published()).toBe('adam@example.test');
  });

  it('closes the dropdown on Escape without clearing the input', async () => {
    setup();
    await type('ad');
    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(2));

    await fireEvent.keyDown(toInput(), { key: 'Escape' });
    expect(screen.queryAllByRole('option')).toHaveLength(0);
    expect(toInput()).toHaveAttribute('aria-expanded', 'false');
    expect(toInput()).toHaveValue('ad');
  });

  it('says so when nothing matches, and still accepts a typed address', async () => {
    setupWithoutSuggestions();
    await type('nobody');
    await waitFor(() => expect(screen.getByRole('status')).toHaveTextContent('No saved contacts'));

    await type('nobody@example.test');
    await fireEvent.keyDown(toInput(), { key: 'Enter' });
    expect(published()).toBe('nobody@example.test');
  });

  it('reports a suggestion failure without blocking composition', async () => {
    const failing: Suggester = {
      cached: () => undefined,
      search: async () => {
        throw new Error('offline');
      },
      cancel: () => {}
    };
    setup({ suggester: failing });

    await type('ad');
    await waitFor(() =>
      expect(screen.getByRole('status')).toHaveTextContent('Suggestions unavailable')
    );

    await type('typed@example.test');
    await fireEvent.keyDown(toInput(), { key: 'Enter' });
    expect(published()).toBe('typed@example.test');
  });

  it('ignores a stale response that lands after a newer query', async () => {
    const { suggester, pending } = manualSuggester();
    setup({ suggester });

    await type('a');
    await type('ada');
    expect(pending).toHaveLength(2);

    // Newest prefix answers first, then the straggler for the abandoned one.
    pending[1].resolve([ADA]);
    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(1));

    pending[0].resolve([ADAM]);
    await new Promise((resolve) => setTimeout(resolve, 0));

    const labels = screen.getAllByRole('option').map((option) => option.textContent ?? '');
    expect(labels).toHaveLength(1);
    expect(labels[0]).toContain('Ada Lovelace');
  });

  it('lets a cached prefix invalidate the request still in flight behind it', async () => {
    // "a" has never been answered; "ada" is already cached. Rendering the
    // cached rows without superseding the older search leaves it the latest
    // sequence, free to repaint the dropdown for a prefix already moved past.
    const { suggester, pending } = manualSuggester({ ada: [ADA] });
    setup({ suggester });

    await type('a');
    expect(pending).toHaveLength(1);

    await type('ada');
    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(1));
    expect(screen.getByText('Ada Lovelace')).toBeInTheDocument();

    pending[0].resolve([ADAM]);
    await new Promise((resolve) => setTimeout(resolve, 0));

    const labels = screen.getAllByRole('option').map((option) => option.textContent ?? '');
    expect(labels).toHaveLength(1);
    expect(labels[0]).toContain('Ada Lovelace');
  });

  // End to end through the real suggester rather than a stub, because the
  // defect lived in the module's cache and a stubbed `cached()` cannot show
  // it. Sending a message makes its recipients suggestible at once; a
  // remembered miss meant the field kept answering "no matches" for the person
  // you had just written to until the tab was reloaded.
  it('offers a just-learned address on the same prefix, with no remount', async () => {
    resetSuggestionCache();
    // Stands in for the address book: empty until the send folds ADAM in.
    let known: AddressSuggestion[] = [];
    const backend = vi.fn(async () => known);
    setup({ suggester: createSuggester(backend) });

    await type('adam');
    await waitFor(() => expect(screen.getByRole('status')).toHaveTextContent('No saved contacts'));
    expect(backend).toHaveBeenCalledTimes(1);

    // The message is sent here; `record_sent_draft_recipients` makes its
    // recipients suggestible at once.
    known = [ADAM];

    // Backing off the prefix and returning to it is the ordinary next
    // keystroke, not a reload.
    await type('ada');
    await type('adam');

    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(1));
    expect(screen.getByText('adam@example.test')).toBeInTheDocument();
    expect(backend, 'the miss was re-asked rather than served from cache').toHaveBeenCalledTimes(
      3
    );
  });
});

describe('RecipientField — account isolation', () => {
  it('drops one account’s rows when the From account changes', async () => {
    const { rerender } = setup();
    await type('ad');
    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(2));

    await rerender({ accountId: 'acc2' });

    expect(screen.queryAllByRole('option')).toHaveLength(0);
    expect(toInput()).toHaveAttribute('aria-expanded', 'false');

    // Closing alone would not be enough: ArrowDown reopens whatever is still
    // held, and Enter would then accept a row scoped to the old account.
    await fireEvent.keyDown(toInput(), { key: 'ArrowDown' });
    expect(screen.queryAllByRole('option')).toHaveLength(0);
    await fireEvent.keyDown(toInput(), { key: 'Enter' });
    expect(published()).toBe('ad');
    expect(published()).not.toContain('ada@example.test');
  });

  it('abandons a request in flight for the account being left', async () => {
    const { suggester, pending } = manualSuggester();
    const { rerender } = setup({ suggester });

    await type('ad');
    expect(pending).toHaveLength(1);

    await rerender({ accountId: 'acc2' });
    pending[0].resolve([ADA, ADAM]);
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(screen.queryAllByRole('option')).toHaveLength(0);
    expect(screen.queryByText('Ada Lovelace')).not.toBeInTheDocument();
  });
});

describe('RecipientField — manual entry and chips', () => {
  it('keeps half-typed input inside the bound value', async () => {
    setupWithoutSuggestions();
    await type('partial@exa');
    expect(published()).toBe('partial@exa');
  });

  it('commits a typed address on comma', async () => {
    setupWithoutSuggestions();
    await type('nobody@example.test');
    await fireEvent.keyDown(toInput(), { key: ',' });

    expect(published()).toBe('nobody@example.test');
    expect(toInput()).toHaveValue('');
    expect(screen.getByRole('button', { name: 'Remove nobody@example.test' })).toBeInTheDocument();
  });

  it('leaves an unusable address in the input instead of dropping it', async () => {
    setupWithoutSuggestions();
    await type('not-an-email');
    await fireEvent.keyDown(toInput(), { key: 'Enter' });

    expect(toInput()).toHaveValue('not-an-email');
    // Still visible to the parent, so its validateAddrs() gate blocks the send.
    expect(published()).toBe('not-an-email');
  });

  it('commits a valid pending address on blur', async () => {
    setupWithoutSuggestions();
    await type('late@example.test');
    await fireEvent.blur(toInput());

    expect(published()).toBe('late@example.test');
    expect(toInput()).toHaveValue('');
  });

  it('renders each recipient as a removable chip', async () => {
    setupWithoutSuggestions({ value: 'Ada Lovelace <ada@example.test>, bare@example.test' });

    expect(screen.getByText('Ada Lovelace')).toBeInTheDocument();
    expect(screen.getByText('bare@example.test')).toBeInTheDocument();

    await fireEvent.click(screen.getByRole('button', { name: 'Remove Ada Lovelace' }));
    expect(published()).toBe('bare@example.test');
  });

  it('removes the last chip with Backspace only when the input is empty', async () => {
    setupWithoutSuggestions({ value: 'a@example.test, b@example.test' });

    await type('x');
    await fireEvent.keyDown(toInput(), { key: 'Backspace' });
    expect(published()).toBe('a@example.test, b@example.test, x');

    await type('');
    await fireEvent.keyDown(toInput(), { key: 'Backspace' });
    expect(published()).toBe('a@example.test');
  });

  it('keeps a quoted display name as one recipient', () => {
    setupWithoutSuggestions({ value: '"Doe, Jane" <jane@example.test>' });
    expect(screen.getAllByRole('button', { name: /^Remove/ })).toHaveLength(1);
    expect(screen.getByText('Doe, Jane')).toBeInTheDocument();
  });

  it('re-parses when the parent replaces the value', async () => {
    const { rerender } = setupWithoutSuggestions({ value: 'first@example.test' });
    expect(screen.getByText('first@example.test')).toBeInTheDocument();

    await rerender({ value: 'second@example.test' });
    expect(screen.getByText('second@example.test')).toBeInTheDocument();
    expect(screen.queryByText('first@example.test')).not.toBeInTheDocument();
  });
});

describe('RecipientField — duplicate suppression', () => {
  it('never offers an address a sibling field already holds', async () => {
    setup({ exclude: ['ada@example.test'] });
    await type('ad');

    await waitFor(() => expect(screen.getAllByRole('option')).toHaveLength(1));
    expect(screen.queryByText('Ada Lovelace')).not.toBeInTheDocument();
    expect(screen.getByText('adam@example.test')).toBeInTheDocument();
  });

  it('refuses a typed duplicate of a sibling field, case-insensitively', async () => {
    setupWithoutSuggestions({ exclude: ['ADA@example.test'] });
    await type('ada@example.test');
    await fireEvent.keyDown(toInput(), { key: ',' });

    expect(published()).toBe('');
    expect(screen.queryAllByRole('button', { name: /^Remove/ })).toHaveLength(0);
  });

  it('refuses a duplicate of an address already in this field', async () => {
    setupWithoutSuggestions({ value: 'ada@example.test' });
    await type('Ada@Example.test');
    await fireEvent.keyDown(toInput(), { key: 'Enter' });

    expect(published()).toBe('ada@example.test');
    expect(screen.getAllByRole('button', { name: /^Remove/ })).toHaveLength(1);
  });
});

describe('RecipientField — multi-recipient commit', () => {
  it('turns a pasted recipient list into one chip per address', async () => {
    setupWithoutSuggestions();
    await paste('ada@example.test, Grace Hopper <grace@example.test>; bob@vendor.test');

    expect(published()).toBe(
      'ada@example.test, Grace Hopper <grace@example.test>, bob@vendor.test'
    );
    expect(screen.getAllByRole('button', { name: /^Remove/ })).toHaveLength(3);
    expect(toInput()).toHaveValue('');
  });

  it('splits a paste that separates recipients by line', async () => {
    setupWithoutSuggestions();
    await paste('ada@example.test\ngrace@example.test\r\nbob@vendor.test');

    expect(published()).toBe('ada@example.test, grace@example.test, bob@vendor.test');
    expect(screen.getAllByRole('button', { name: /^Remove/ })).toHaveLength(3);
  });

  it('keeps a quoted display name carrying a comma as one pasted chip', async () => {
    setupWithoutSuggestions();
    await paste('"Doe, Jane" <jane@example.test>, bob@vendor.test');

    expect(screen.getAllByRole('button', { name: /^Remove/ })).toHaveLength(2);
    expect(screen.getByText('Doe, Jane')).toBeInTheDocument();
  });

  it('drops a pasted address a sibling field already holds', async () => {
    setupWithoutSuggestions({ exclude: ['ADA@example.test'] });
    await paste('ada@example.test, grace@example.test, Ada@example.test');

    expect(published()).toBe('grace@example.test');
    expect(screen.getAllByRole('button', { name: /^Remove/ })).toHaveLength(1);
  });

  it('leaves an unusable fragment in the input while committing its neighbours', async () => {
    setupWithoutSuggestions();
    await paste('good@example.test, not-an-email, also@example.test');

    expect(toInput()).toHaveValue('not-an-email');
    // Still visible to the parent, so validateAddrs() keeps the send blocked.
    expect(published()).toBe('good@example.test, also@example.test, not-an-email');
    expect(screen.getAllByRole('button', { name: /^Remove/ })).toHaveLength(2);
  });

  // A pasted entry carrying syntax after its angle address is not a recipient:
  // `lettre::Mailboxes` parses the whole header value and fails on the leftover
  // text. Extracting the brackets with a substring match found the address
  // inside and chipped it, so the composer's send button lit up for a draft
  // that died at SMTP. It has to stay in the input, blocking the send.
  it('refuses a pasted entry with syntax left over after the angle address', async () => {
    setupWithoutSuggestions();
    await paste('Ada <ada@example.test> trailing, bob@vendor.test');

    expect(screen.getAllByRole('button', { name: /^Remove/ })).toHaveLength(1);
    expect(toInput()).toHaveValue('Ada <ada@example.test> trailing');
    expect(published()).toBe('bob@vendor.test, Ada <ada@example.test> trailing');
  });

  // The composer's size limits are UTF-8 byte limits, and its domains are
  // ASCII: both are what the send edge enforces. Counting UTF-16 code units or
  // waving Unicode domains through chipped recipients that only failed on the
  // wire.
  it('refuses a pasted recipient the send edge would measure or spell differently', async () => {
    setupWithoutSuggestions();
    await paste(`${'é'.repeat(33)}@example.test, ada@exämple.test, ada@example.test`);

    expect(screen.getAllByRole('button', { name: /^Remove/ })).toHaveLength(1);
    expect(toInput()).toHaveValue(`${'é'.repeat(33)}@example.test, ada@exämple.test`);
  });

  it('commits a multi-recipient value left in the input on blur', async () => {
    setupWithoutSuggestions();
    await type('a@example.test, b@example.test');
    await fireEvent.blur(toInput());

    expect(published()).toBe('a@example.test, b@example.test');
    expect(screen.getAllByRole('button', { name: /^Remove/ })).toHaveLength(2);
    expect(toInput()).toHaveValue('');
  });

  it('keeps an unusable fragment on blur too', async () => {
    setupWithoutSuggestions({ exclude: ['dup@example.test'] });
    await type('dup@example.test, fine@example.test, still-typ');
    await fireEvent.blur(toInput());

    expect(published()).toBe('fine@example.test, still-typ');
    expect(toInput()).toHaveValue('still-typ');
  });
});

describe('RecipientField — accessibility and disabled state', () => {
  it('exposes combobox semantics wired to its listbox', () => {
    setup();
    const input = toInput();
    expect(input).toHaveAttribute('aria-autocomplete', 'list');
    expect(input).toHaveAttribute('aria-controls', 'field-to-listbox');
    expect(input).toHaveAttribute('aria-expanded', 'false');
    expect(screen.getByRole('listbox')).toHaveAttribute('id', 'field-to-listbox');
  });

  it('associates its label with the input', () => {
    setup();
    expect(screen.getByLabelText('To')).toBe(toInput());
  });

  it('marks itself invalid for the parent validation gate', () => {
    setup({ invalid: true });
    expect(toInput()).toHaveAttribute('aria-invalid', 'true');
  });

  it('disables the input and the chip removers while locked', () => {
    setup({ value: 'a@example.test', disabled: true });
    expect(toInput()).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Remove a@example.test' })).toBeDisabled();
  });
});
