<script lang="ts">
  // A draft awaiting approval. Subject + source chip + age, then contextual
  // actions: Approve (primary) / Edit (ghost) / Discard (danger-ghost). Each
  // action calls back to the parent, which hits the existing per-account draft
  // endpoint via cockpitApi.draftAction(draft.action_base, …).
  import Badge from './Badge.svelte';
  import Button from './Button.svelte';
  import MonoTag from './MonoTag.svelte';
  import type { ApprovalDraft } from '$lib/cockpit-api';

  let {
    draft,
    age,
    busy = false,
    onapprove,
    onedit,
    ondiscard
  }: {
    draft: ApprovalDraft;
    age: (iso: string | null) => string;
    busy?: boolean;
    onapprove: (draft: ApprovalDraft) => void;
    onedit: (draft: ApprovalDraft) => void;
    ondiscard: (draft: ApprovalDraft) => void;
  } = $props();
</script>

<div class="approval-row" class:busy>
  <div class="approval-meta">
    <span class="approval-subject">{draft.subject || '(no subject)'}</span>
    <span class="approval-sub">
      {#if draft.created_by}
        <MonoTag>{draft.created_by}</MonoTag>
      {/if}
      {#if draft.status === 'blocked'}
        <Badge variant="danger">blocked</Badge>
      {/if}
      <span class="approval-age">{age(draft.updated_at)}</span>
    </span>
  </div>
  <div class="approval-actions">
    <Button variant="primary" disabled={busy} onclick={() => onapprove(draft)}>Approve</Button>
    <Button variant="ghost" disabled={busy} onclick={() => onedit(draft)}>Edit</Button>
    <Button variant="danger" disabled={busy} onclick={() => ondiscard(draft)}>Discard</Button>
  </div>
</div>

<style>
  .approval-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.65rem 0.75rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    background: var(--env-surface);
  }
  .approval-row.busy {
    opacity: 0.6;
  }
  .approval-meta {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    min-width: 0;
  }
  .approval-subject {
    font-size: 0.875rem;
    font-weight: 600;
    color: var(--env-ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .approval-sub {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
  }
  .approval-age {
    font-size: 0.75rem;
    color: var(--env-muted);
  }
  .approval-actions {
    display: inline-flex;
    gap: 0.4rem;
    flex-shrink: 0;
  }
</style>
