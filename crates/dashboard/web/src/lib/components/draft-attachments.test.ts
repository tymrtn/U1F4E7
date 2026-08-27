// Tests for the draft review page's attachment surface.
//
// Coverage:
//   • attachments render as named, sized, downloadable chips (the count-only
//     note this replaced showed "3 attachments" and nothing else)
//   • attaching sends the viewed expected_revision and the base64 payload
//   • attaching mid-edit keeps unsaved editor text and adopts the new revision
//   • removing detaches by name at the current revision
//   • a 409 raises the composer's conflict banner instead of clobbering
//   • the size ceiling is refused client-side, before reading the file
//   • non-editable drafts keep the list and lose the controls

import { render, screen, fireEvent, waitFor, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { page as pageState } from '$app/state';

const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    draft: vi.fn(),
    editDraft: vi.fn(),
    sendDraft: vi.fn(),
    discardDraft: vi.fn(),
    addressSuggestions: vi.fn(),
    uploadDraftAttachments: vi.fn(),
    deleteDraftAttachment: vi.fn()
  }
}));

vi.mock('$lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('$lib/api')>();
  return { ...actual, api: { ...actual.api, ...apiMock } };
});

import DraftComposer from './DraftComposer.svelte';
import { EnvelopeApiError, MAX_DRAFT_ATTACHMENT_BYTES, type Draft } from '$lib/api';

const ACCOUNT = '31f5fddf-04f9-4978-aea5-29aa9af12bb0';
const DRAFT = '365d958c-6666-4872-898e-cb8a60f21aca';

const BASE_DRAFT: Draft = {
  id: DRAFT,
  account_id: ACCOUNT,
  status: 'draft',
  to_addr: 'alexander@example.com',
  cc_addr: null,
  bcc_addr: null,
  reply_to: null,
  subject: 'Expatriator CBI interview test cases',
  text_content: 'Hi Alexander,',
  html_content: null,
  in_reply_to: null,
  metadata: null,
  attachments: [],
  message_id: null,
  send_after: null,
  snoozed_until: null,
  created_at: '2026-08-17T10:00:00Z',
  updated_at: '2026-08-17T10:00:00Z',
  sent_at: null,
  created_by: 'agent',
  revision: 7
};

const THREE_FILES = [
  { filename: 'case-one.md', content_type: 'text/markdown', size: 2048 },
  { filename: 'case-two.md', content_type: 'text/markdown', size: 4096 },
  { filename: 'case-three.md', content_type: 'text/markdown', size: 1024 }
];

async function renderLoaded(overrides: Partial<Draft> = {}) {
  apiMock.draft.mockResolvedValue({ draft: { ...BASE_DRAFT, ...overrides } });
  render(DraftComposer);
  await waitFor(() => expect(screen.getByLabelText('To')).toBeInTheDocument());
}

function panel(): HTMLElement {
  return document.getElementById('draft-attachments') as HTMLElement;
}

function chipNames(): string[] {
  return Array.from(panel().querySelectorAll('.chip-name')).map((n) => n.textContent?.trim() ?? '');
}

/** A File whose bytes are readable by the component's arrayBuffer() path. */
function file(name: string, bytes: number, type = 'text/markdown'): File {
  return new File([new Uint8Array(bytes)], name, { type });
}

/** Drive the hidden file input the way a picker would. */
async function pick(files: File[]) {
  const input = document.getElementById('draft-attachment-input') as HTMLInputElement;
  Object.defineProperty(input, 'files', { value: files, configurable: true });
  await fireEvent.change(input);
}

beforeEach(() => {
  pageState.params = { account: ACCOUNT, draft: DRAFT };
  pageState.url = new URL(
    `http://localhost/accounts/${ACCOUNT}/drafts/${DRAFT}`
  ) as typeof pageState.url;

  apiMock.draft.mockResolvedValue({ draft: BASE_DRAFT });
  apiMock.addressSuggestions.mockResolvedValue({
    account_id: ACCOUNT,
    query: '',
    limit: 8,
    suggestions: []
  });
  apiMock.uploadDraftAttachments.mockImplementation(
    (_a: string, _d: string, body: { expected_revision: number; attachments: unknown[] }) =>
      Promise.resolve({
        draft: {
          ...BASE_DRAFT,
          revision: body.expected_revision + 1,
          attachments: (body.attachments as { filename: string; content_type: string }[]).map(
            (a) => ({ filename: a.filename, content_type: a.content_type, size: 10 })
          )
        },
        status: 'attached'
      })
  );
  apiMock.deleteDraftAttachment.mockImplementation(
    (_a: string, _d: string, filename: string, revision: number) =>
      Promise.resolve({
        draft: {
          ...BASE_DRAFT,
          revision: revision + 1,
          attachments: THREE_FILES.filter((a) => a.filename !== filename)
        },
        status: 'detached'
      })
  );
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('draft attachment list', () => {
  it('renders each attachment with its name, type, size, and download link', async () => {
    await renderLoaded({ attachments: THREE_FILES });

    expect(chipNames()).toEqual(['case-one.md', 'case-two.md', 'case-three.md']);
    expect(within(panel()).getByText('2 KB')).toBeInTheDocument();
    expect(within(panel()).getAllByText('text/markdown')).toHaveLength(3);

    const link = within(panel()).getByLabelText('Download case-one.md');
    expect(link).toHaveAttribute(
      'href',
      `/api/accounts/${ACCOUNT}/drafts/${DRAFT}/attachments/case-one.md`
    );
    expect(link).toHaveAttribute('download', 'case-one.md');
  });

  it('summarises the count and total size', async () => {
    await renderLoaded({ attachments: THREE_FILES });
    expect(within(panel()).getByText(/3 files · 7 KB/)).toBeInTheDocument();
  });

  it('says so plainly when a draft has no attachments', async () => {
    await renderLoaded();
    expect(within(panel()).getByText('None')).toBeInTheDocument();
    expect(chipNames()).toEqual([]);
  });
});

describe('attaching', () => {
  it('uploads the picked file at the viewed revision', async () => {
    await renderLoaded();
    await pick([file('case-one.md', 8)]);

    await waitFor(() => expect(apiMock.uploadDraftAttachments).toHaveBeenCalled());
    const [account, draftId, body] = apiMock.uploadDraftAttachments.mock.calls[0];
    expect(account).toBe(ACCOUNT);
    expect(draftId).toBe(DRAFT);
    expect(body.expected_revision).toBe(7);
    expect(body.attachments).toEqual([
      { filename: 'case-one.md', content_type: 'text/markdown', data_b64: btoa('\0'.repeat(8)) }
    ]);

    await waitFor(() => expect(chipNames()).toEqual(['case-one.md']));
  });

  it('attaches every file from a multi-select in one call', async () => {
    await renderLoaded();
    await pick([file('a.md', 4), file('b.md', 4)]);

    await waitFor(() => expect(chipNames()).toEqual(['a.md', 'b.md']));
    expect(apiMock.uploadDraftAttachments).toHaveBeenCalledTimes(1);
  });

  /**
   * The regression that makes this surface usable at all: the server's
   * post-attach draft carries the PRE-edit body, so adopting it wholesale
   * would wipe whatever the operator had typed but not yet saved.
   */
  it('keeps unsaved body text and adopts the new revision', async () => {
    await renderLoaded();
    const body = screen.getByLabelText('Message');
    await fireEvent.input(body, { target: { value: 'Hi Alexander,\n\nThree test cases:' } });

    await pick([file('case-one.md', 8)]);
    await waitFor(() => expect(chipNames()).toEqual(['case-one.md']));

    expect((body as HTMLTextAreaElement).value).toBe('Hi Alexander,\n\nThree test cases:');
    expect(screen.getByText(/Unsaved changes/)).toBeInTheDocument();

    // The next save must echo the revision the attachment write produced (8),
    // not the one the page loaded with (7).
    await fireEvent.click(screen.getByRole('button', { name: 'Save changes' }));
    await waitFor(() => expect(apiMock.editDraft).toHaveBeenCalled());
    expect(apiMock.editDraft.mock.calls[0][2].expected_revision).toBe(8);
  });

  it('refuses an over-limit file without calling the API', async () => {
    await renderLoaded();
    await pick([file('huge.bin', MAX_DRAFT_ATTACHMENT_BYTES + 1, 'application/octet-stream')]);

    await waitFor(() => expect(within(panel()).getByRole('alert')).toBeInTheDocument());
    expect(within(panel()).getByRole('alert').textContent).toMatch(/limit is left on this draft/);
    expect(apiMock.uploadDraftAttachments).not.toHaveBeenCalled();
  });

  it('counts what is already attached against the ceiling', async () => {
    await renderLoaded({
      attachments: [
        {
          filename: 'big.bin',
          content_type: 'application/octet-stream',
          size: MAX_DRAFT_ATTACHMENT_BYTES - 100
        }
      ]
    });
    await pick([file('small.bin', 200, 'application/octet-stream')]);

    await waitFor(() => expect(within(panel()).getByRole('alert')).toBeInTheDocument());
    expect(apiMock.uploadDraftAttachments).not.toHaveBeenCalled();
  });

  it('surfaces a server rejection without dropping the list', async () => {
    await renderLoaded({ attachments: THREE_FILES });
    apiMock.uploadDraftAttachments.mockRejectedValue(
      new EnvelopeApiError(413, 'http_413', 'attachments would total too many bytes', undefined)
    );

    await pick([file('one-more.md', 8)]);
    await waitFor(() =>
      expect(within(panel()).getByRole('alert').textContent).toMatch(/too many bytes/)
    );
    expect(chipNames()).toEqual(['case-one.md', 'case-two.md', 'case-three.md']);
  });
});

describe('removing', () => {
  it('detaches by name at the current revision', async () => {
    await renderLoaded({ attachments: THREE_FILES });
    await fireEvent.click(within(panel()).getByLabelText('Remove case-two.md'));

    await waitFor(() => expect(apiMock.deleteDraftAttachment).toHaveBeenCalled());
    expect(apiMock.deleteDraftAttachment).toHaveBeenCalledWith(ACCOUNT, DRAFT, 'case-two.md', 7);
    await waitFor(() => expect(chipNames()).toEqual(['case-one.md', 'case-three.md']));
  });
});

describe('conflicts and read-only drafts', () => {
  it('raises the composer conflict banner on a 409', async () => {
    await renderLoaded({ attachments: THREE_FILES });
    apiMock.deleteDraftAttachment.mockRejectedValue(
      new EnvelopeApiError(409, 'draft_modified', 'draft changed', undefined)
    );

    await fireEvent.click(within(panel()).getByLabelText('Remove case-one.md'));

    await waitFor(() =>
      expect(document.getElementById('draft-conflict')).toBeInTheDocument()
    );
    // The list is untouched: nothing was removed.
    expect(chipNames()).toEqual(['case-one.md', 'case-two.md', 'case-three.md']);
  });

  it('keeps the list but drops the controls once a draft is read-only', async () => {
    await renderLoaded({ status: 'sent', attachments: THREE_FILES });

    expect(chipNames()).toEqual(['case-one.md', 'case-two.md', 'case-three.md']);
    expect(within(panel()).getByLabelText('Download case-one.md')).toBeInTheDocument();
    expect(within(panel()).queryByRole('button', { name: 'Attach files' })).toBeNull();
    expect(within(panel()).queryByLabelText('Remove case-one.md')).toBeNull();
  });

  it('drops the controls while a draft sits in the outbox', async () => {
    await renderLoaded({ send_after: '2026-08-17T10:02:00Z', attachments: THREE_FILES });

    expect(chipNames()).toEqual(['case-one.md', 'case-two.md', 'case-three.md']);
    expect(within(panel()).queryByRole('button', { name: 'Attach files' })).toBeNull();
    expect(within(panel()).queryByLabelText('Remove case-one.md')).toBeNull();
  });
});
