<script lang="ts">
  // Draft attachments — the review page's attachment surface: see what is
  // attached, download any of it, attach more, remove what should not go.
  //
  // Two things distinguish this from the reader's AttachmentList:
  //
  //   • It mutates. Attaching and detaching change what will be SENT, so both
  //     carry `expected_revision` and bump the draft's revision. The parent
  //     adopts the returned draft's revision and attachment list WITHOUT
  //     touching the editor fields, so attaching a file mid-sentence never
  //     discards typed text (see `adoptAttachmentChange` in DraftComposer).
  //   • Bytes are never in hand. The draft JSON carries metadata only; each
  //     filename resolves to a download URL the backend streams on demand.

  import {
    MAX_DRAFT_ATTACHMENT_BYTES,
    api,
    draftAttachmentDownloadUrl,
    type Draft,
    type DraftAttachment,
    type EnvelopeApiError
  } from '$lib/api';
  import { formatBytes } from '$lib/reader-api';
  import MonoTag from './MonoTag.svelte';
  import Spinner from './Spinner.svelte';

  interface Props {
    draft: Draft;
    accountId: string;
    /** False for queued/sent/sending drafts — the list stays, the controls go. */
    editable: boolean;
    /** Hand the parent the server's post-mutation draft. */
    onchange: (draft: Draft) => void;
    /** Surface a 409 through the parent's existing conflict banner. */
    onconflict: () => void;
  }

  let { draft, accountId, editable, onchange, onconflict }: Props = $props();

  let busy = $state(false);
  let dragging = $state(false);
  let error = $state<string | null>(null);
  let fileInput = $state<HTMLInputElement | null>(null);

  const attachments = $derived(draft.attachments ?? []);
  const totalBytes = $derived(attachments.reduce((sum, a) => sum + (a.size ?? 0), 0));
  const remainingBytes = $derived(Math.max(0, MAX_DRAFT_ATTACHMENT_BYTES - totalBytes));

  function mimeBase(contentType: string): string {
    return (contentType || 'application/octet-stream').split(';')[0].trim();
  }

  function downloadUrl(attachment: DraftAttachment): string {
    return draftAttachmentDownloadUrl(accountId, draft.id, attachment.filename);
  }

  /**
   * Base64 a File without blowing the call stack.
   *
   * `String.fromCharCode(...bytes)` on a multi-megabyte file exceeds the
   * argument limit, so the bytes go across in 32KB chunks — same approach as
   * the compose drawer.
   */
  async function encode(file: File): Promise<string> {
    const bytes = new Uint8Array(await file.arrayBuffer());
    let binary = '';
    const chunkSize = 0x8000;
    for (let index = 0; index < bytes.length; index += chunkSize) {
      binary += String.fromCharCode(...bytes.subarray(index, index + chunkSize));
    }
    return btoa(binary);
  }

  async function attach(files: File[]) {
    if (!editable || busy || files.length === 0) return;
    error = null;

    // Check the ceiling here as well as server-side: reading and base64ing a
    // 60MB file only to be told no is a slow way to learn it.
    const incoming = files.reduce((sum, file) => sum + file.size, 0);
    if (incoming > remainingBytes) {
      error = `That is ${formatBytes(incoming)}, and only ${formatBytes(remainingBytes)} of the ${formatBytes(MAX_DRAFT_ATTACHMENT_BYTES)} limit is left on this draft.`;
      return;
    }

    busy = true;
    try {
      const payload = await Promise.all(
        files.map(async (file) => ({
          filename: file.name,
          content_type: file.type || 'application/octet-stream',
          data_b64: await encode(file)
        }))
      );
      const res = await api.uploadDraftAttachments(accountId, draft.id, {
        expected_revision: draft.revision,
        attachments: payload
      });
      onchange(res.draft);
    } catch (e) {
      handle(e, 'Could not attach that file.');
    } finally {
      busy = false;
    }
  }

  async function detach(attachment: DraftAttachment) {
    if (!editable || busy) return;
    error = null;
    busy = true;
    try {
      const res = await api.deleteDraftAttachment(
        accountId,
        draft.id,
        attachment.filename,
        draft.revision
      );
      onchange(res.draft);
    } catch (e) {
      handle(e, `Could not remove ${attachment.filename}.`);
    } finally {
      busy = false;
    }
  }

  /** 409 is the revision guard — hand it to the parent's conflict banner. */
  function handle(e: unknown, fallback: string) {
    const err = e as EnvelopeApiError;
    if (err.status === 409) {
      onconflict();
      return;
    }
    error = err.message ?? fallback;
  }

  function onFilePicked(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const files = Array.from(input.files ?? []);
    input.value = '';
    void attach(files);
  }

  function onDrop(event: DragEvent) {
    event.preventDefault();
    dragging = false;
    if (!editable) return;
    void attach(Array.from(event.dataTransfer?.files ?? []));
  }

  function onDragOver(event: DragEvent) {
    if (!editable) return;
    event.preventDefault();
    dragging = true;
  }
</script>

<section
  class="draft-attachments"
  class:is-dragging={dragging}
  id="draft-attachments"
  aria-label="Attachments"
  ondrop={onDrop}
  ondragover={onDragOver}
  ondragleave={() => (dragging = false)}
>
  <header class="attachments-head">
    <h3 class="attachments-title">Attachments</h3>
    <span class="attachments-count">
      {#if attachments.length === 0}
        None
      {:else}
        {attachments.length}
        {attachments.length === 1 ? 'file' : 'files'} · {formatBytes(totalBytes)}
      {/if}
    </span>
    {#if editable}
      <button
        type="button"
        class="attachments-add"
        disabled={busy}
        onclick={() => fileInput?.click()}
      >
        {#if busy}<Spinner label="Attaching" />{/if}
        {busy ? 'Attaching' : 'Attach files'}
      </button>
      <input
        bind:this={fileInput}
        id="draft-attachment-input"
        type="file"
        multiple
        hidden
        onchange={onFilePicked}
      />
    {/if}
  </header>

  {#if attachments.length > 0}
    <ul class="attachments-items">
      {#each attachments as attachment (attachment.filename)}
        <li class="attachment-chip">
          <a
            class="chip-link"
            href={downloadUrl(attachment)}
            download={attachment.filename}
            aria-label="Download {attachment.filename}"
          >
            <span class="chip-name">{attachment.filename}</span>
            <MonoTag>{mimeBase(attachment.content_type)}</MonoTag>
            <span class="chip-size">{formatBytes(attachment.size)}</span>
          </a>
          {#if editable}
            <button
              type="button"
              class="chip-remove"
              disabled={busy}
              aria-label="Remove {attachment.filename}"
              onclick={() => detach(attachment)}>×</button
            >
          {/if}
        </li>
      {/each}
    </ul>
  {/if}

  {#if error}
    <p class="attachments-error" role="alert">{error}</p>
  {:else if editable}
    <p class="attachments-hint">
      {#if attachments.length === 0}
        Drop files here, or use Attach files. Up to {formatBytes(MAX_DRAFT_ATTACHMENT_BYTES)} per message.
      {:else}
        Adding or removing a file changes what gets sent, so it needs approving again before it
        goes.
      {/if}
    </p>
  {/if}
</section>

<style>
  .draft-attachments {
    min-width: 0;
    border: 1px solid var(--env-rule);
    background: var(--env-surface);
    padding: 0.75rem 0.875rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .draft-attachments.is-dragging {
    border-color: var(--env-accent);
    background: var(--env-accent-soft);
  }

  .attachments-head {
    display: flex;
    align-items: center;
    gap: 0.6rem;
  }
  .attachments-title {
    margin: 0;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
  }
  .attachments-count {
    flex: 1;
    min-width: 0;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
  .attachments-add {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    flex-shrink: 0;
    border: 1px solid var(--env-rule);
    background: var(--env-soft);
    color: var(--env-ink);
    font-size: 0.75rem;
    padding: 0.25rem 0.6rem;
    cursor: pointer;
  }
  .attachments-add:hover:not(:disabled) {
    border-color: var(--env-accent);
    color: var(--env-accent);
  }
  .attachments-add:disabled {
    cursor: default;
    color: var(--env-muted);
  }

  .attachments-items {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
  }
  .attachment-chip {
    display: inline-flex;
    align-items: center;
    max-width: 100%;
    border: 1px solid var(--env-rule);
    background: var(--env-soft);
  }
  .chip-link {
    display: inline-flex;
    align-items: baseline;
    gap: 0.45rem;
    min-width: 0;
    padding: 0.28rem 0.55rem;
    font-size: 0.8125rem;
    color: var(--env-accent);
    text-decoration: none;
  }
  .chip-link:hover {
    text-decoration: underline;
  }
  .chip-name {
    font-weight: 500;
    overflow-wrap: anywhere;
  }
  .chip-size {
    flex-shrink: 0;
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
  .chip-remove {
    flex-shrink: 0;
    align-self: stretch;
    border: 0;
    border-left: 1px solid var(--env-rule);
    background: transparent;
    color: var(--env-muted);
    font-size: 0.9375rem;
    line-height: 1;
    padding: 0 0.45rem;
    cursor: pointer;
  }
  .chip-remove:hover:not(:disabled) {
    background: var(--env-warn-soft);
    color: var(--env-warn);
  }
  .chip-remove:disabled {
    cursor: default;
    opacity: 0.5;
  }

  .attachments-hint,
  .attachments-error {
    margin: 0;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    line-height: 1.5;
    color: var(--env-muted);
  }
  .attachments-error {
    color: var(--env-warn);
  }
</style>
