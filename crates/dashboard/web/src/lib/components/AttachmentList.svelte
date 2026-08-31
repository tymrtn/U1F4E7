<script lang="ts">
  // AttachmentList — renders message attachments with filename, size, and download.

  import type { AttachmentMeta } from '$lib/reader-api';
  import { attachmentDownloadUrl, formatBytes } from '$lib/reader-api';
  import Icon from '$lib/components/Icon.svelte';

  interface Props {
    attachments: AttachmentMeta[];
    accountId: string;
    uid: number;
    folder?: string;
  }

  let { attachments, accountId, uid, folder = 'INBOX' }: Props = $props();

  function downloadUrl(a: AttachmentMeta): string {
    return attachmentDownloadUrl(accountId, uid, a.filename, folder);
  }

  function mimeBase(ct: string): string {
    return ct.split(';')[0].trim();
  }
</script>

{#if attachments.length > 0}
  <section class="attachment-list" id="attachment-list" aria-label="Attachments">
    <h3 class="attachment-heading">
      {attachments.length} attachment{attachments.length === 1 ? '' : 's'}
    </h3>
    <ul class="attachment-items">
      {#each attachments as a (a.filename + a.size)}
        <li class="attachment-item">
          <a
            class="attachment-card"
            href={downloadUrl(a)}
            download={a.filename}
            aria-label="Download {a.filename}"
          >
            <span class="attachment-icon" aria-hidden="true"><Icon name="paperclip" size={16} /></span>
            <span class="attachment-body">
              <span class="attachment-name">{a.filename}</span>
              <span class="attachment-meta">{mimeBase(a.content_type)} · {formatBytes(a.size)}</span>
            </span>
          </a>
        </li>
      {/each}
    </ul>
  </section>
{/if}

<style>
  .attachment-list {
    margin-top: 1rem;
    padding-top: 0.75rem;
    border-top: 1px solid var(--env-rule);
  }
  .attachment-heading {
    margin: 0 0 0.5rem;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
  }
  .attachment-items {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }
  .attachment-item {
    display: flex;
  }
  .attachment-card {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    max-width: 22rem;
    padding: 0.5rem 0.7rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-md, 5px);
    background: var(--env-surface);
    text-decoration: none;
    color: var(--env-ink);
    transition: border-color 0.1s ease, background 0.1s ease;
  }
  .attachment-card:hover {
    border-color: var(--env-accent);
    background: var(--env-accent-soft);
  }
  .attachment-icon {
    display: inline-flex;
    color: var(--env-muted);
    flex-shrink: 0;
  }
  .attachment-card:hover .attachment-icon {
    color: var(--env-accent);
  }
  .attachment-body {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .attachment-name {
    font-size: 0.8125rem;
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .attachment-meta {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
</style>
