<script lang="ts">
  // AttachmentList — renders message attachments with filename, size, and download.

  import type { AttachmentMeta } from '$lib/reader-api';
  import { attachmentDownloadUrl, formatBytes } from '$lib/reader-api';
  import { MonoTag } from '$lib/components';

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
    <h3 class="attachment-heading">Attachments</h3>
    <ul class="attachment-items">
      {#each attachments as a (a.filename + a.size)}
        <li class="attachment-item">
          <a
            class="attachment-link"
            href={downloadUrl(a)}
            download={a.filename}
            aria-label="Download {a.filename}"
          >
            <span class="attachment-name">{a.filename}</span>
            <span class="attachment-meta">
              <MonoTag>{mimeBase(a.content_type)}</MonoTag>
              <span class="attachment-size">{formatBytes(a.size)}</span>
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
    flex-direction: column;
    gap: 0.3rem;
  }
  .attachment-item {
    display: flex;
  }
  .attachment-link {
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    font-size: 0.8125rem;
    color: var(--env-accent);
    text-decoration: none;
    padding: 0.2rem 0;
  }
  .attachment-link:hover {
    text-decoration: underline;
  }
  .attachment-name {
    font-weight: 500;
    overflow-wrap: anywhere;
  }
  .attachment-meta {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    flex-shrink: 0;
  }
  .attachment-size {
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
</style>
