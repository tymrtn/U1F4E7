<script lang="ts">
  // ThreatBanner — the reader's threat verdict: level, "Why?" (the score
  // arithmetic), "Mark safe", and "Report" (drafts a report with the original
  // attached; never sends). Clean mail shows nothing.
  import { base } from '$app/paths';
  import {
    postThreatMarkSafe,
    postThreatReport,
    type ThreatView
  } from '$lib/reader-api';

  let {
    threat,
    accountId,
    uid,
    folder,
    onchange
  }: {
    threat: ThreatView;
    accountId: string;
    uid: number;
    folder: string;
    onchange?: (next: ThreatView) => void;
  } = $props();

  let whyOpen = $state(false);
  let busy = $state<'safe' | 'report' | null>(null);
  let failure = $state<string | null>(null);
  let reportDraftId = $state<string | null>(null);

  const visible = $derived(threat.level !== 'clean' || threat.marked_safe === true);

  const heading = $derived.by(() => {
    if (threat.marked_safe) return 'You marked this message safe';
    switch (threat.level) {
      case 'dangerous':
        return 'This message looks dangerous';
      case 'suspicious':
        return 'This message looks suspicious';
      default:
        return 'The threat check could not finish';
    }
  });

  const detail = $derived.by(() => {
    if (threat.marked_safe) return 'Envelope will not flag it or block its attachments.';
    if (threat.level === 'unavailable') {
      return threat.error
        ? `Treat it with care: ${threat.error}`
        : 'A required check failed, so Envelope cannot vouch for it.';
    }
    const parts: string[] = [];
    if (threat.level === 'dangerous') parts.push('Do not open its links or reply with anything private.');
    else parts.push('Check the sender and links before you act on it.');
    if (threat.malware) parts.push('Its attachments are blocked from download.');
    return parts.join(' ');
  });

  async function markSafe() {
    busy = 'safe';
    failure = null;
    try {
      const res = await postThreatMarkSafe(accountId, uid, folder);
      onchange?.(res.threat);
    } catch (e) {
      failure = (e as Error)?.message ?? 'Could not mark this message safe.';
    } finally {
      busy = null;
    }
  }

  async function report() {
    busy = 'report';
    failure = null;
    try {
      const res = await postThreatReport(accountId, uid, folder);
      reportDraftId = res.draft_id;
    } catch (e) {
      failure = (e as Error)?.message ?? 'Could not draft the report.';
    } finally {
      busy = null;
    }
  }
</script>

{#if visible}
  <section
    class="threat-banner threat-{threat.marked_safe ? 'safe' : threat.level}"
    id="threat-banner"
    role={threat.level === 'dangerous' && !threat.marked_safe ? 'alert' : 'status'}
    aria-label="Threat check"
  >
    <div class="threat-line">
      <p class="threat-heading">{heading}</p>
      {#if typeof threat.score === 'number' && threat.level !== 'unavailable'}
        <span class="threat-score" title="Threat score out of 100">{threat.score}/100</span>
      {/if}
    </div>
    <p class="threat-detail">{detail}</p>

    <div class="threat-actions">
      {#if threat.explain && threat.explain.length > 0}
        <button
          class="threat-btn"
          type="button"
          aria-expanded={whyOpen}
          aria-controls="threat-why"
          onclick={() => (whyOpen = !whyOpen)}
        >
          Why?
        </button>
      {/if}
      {#if !threat.marked_safe && threat.level !== 'unavailable'}
        <button class="threat-btn" type="button" disabled={busy !== null} onclick={markSafe}>
          {busy === 'safe' ? 'Marking…' : 'Mark safe'}
        </button>
      {/if}
      {#if reportDraftId}
        <a
          class="threat-report-done"
          href="{base}/accounts/{encodeURIComponent(accountId)}/drafts/{encodeURIComponent(reportDraftId)}"
        >
          Report drafted, not sent. Review it
        </a>
      {:else if !threat.marked_safe}
        <button class="threat-btn" type="button" disabled={busy !== null} onclick={report}>
          {busy === 'report' ? 'Drafting…' : 'Report'}
        </button>
      {/if}
    </div>

    {#if whyOpen && threat.explain}
      <ol class="threat-why" id="threat-why">
        {#each threat.explain as line, i (i)}
          <li>{line}</li>
        {/each}
      </ol>
    {/if}
    {#if failure}
      <p class="threat-failure" role="alert">{failure}</p>
    {/if}
  </section>
{/if}

<style>
  .threat-banner {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    margin: 0 0 0.9rem;
    padding: 0.75rem 0.95rem;
    border: 1px solid var(--env-rule);
    border-left-width: 3px;
    border-radius: var(--radius-xs, 2px);
    background: var(--env-surface);
  }
  .threat-dangerous {
    border-color: var(--env-warn);
    background: var(--env-warn-soft);
  }
  .threat-suspicious {
    border-color: var(--env-pending);
    background: var(--env-pending-soft);
  }
  .threat-unavailable,
  .threat-safe {
    border-color: var(--env-rule);
    background: var(--env-soft);
  }
  .threat-line {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 0.75rem;
  }
  .threat-heading {
    margin: 0;
    font-size: 0.9375rem;
    font-weight: 600;
    color: var(--env-ink);
  }
  .threat-dangerous .threat-heading {
    color: var(--env-warn);
  }
  .threat-score {
    font-family: var(--font-mono);
    font-size: 0.75rem;
    color: var(--env-muted);
    white-space: nowrap;
  }
  .threat-detail {
    margin: 0;
    font-size: 0.8125rem;
    line-height: 1.45;
    color: var(--env-ink);
  }
  .threat-actions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.5rem;
  }
  .threat-btn {
    font-size: 0.8125rem;
    padding: 0.2rem 0.6rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    background: var(--env-paper);
    color: var(--env-ink);
    cursor: pointer;
  }
  .threat-btn:hover:not(:disabled) {
    border-color: var(--env-ink);
  }
  .threat-btn:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .threat-report-done {
    font-size: 0.8125rem;
    color: var(--env-accent);
  }
  .threat-why {
    margin: 0.2rem 0 0;
    padding: 0.5rem 0.65rem 0.5rem 1.6rem;
    font-family: var(--font-mono);
    font-size: 0.75rem;
    line-height: 1.55;
    color: var(--env-ink);
    background: var(--env-paper);
    border: 1px solid var(--env-rule-soft);
    border-radius: var(--radius-xs, 2px);
    overflow-x: auto;
  }
  .threat-failure {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-warn);
  }
</style>
