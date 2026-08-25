<script lang="ts">
  // Envelope v2 Agent Cockpit — the launch demo screen.
  //
  // "agent fleet on shared inbox — every action attributed."
  //
  // An overview grid of read-only aggregate surfaces: the per-agent attribution
  // feed (agent cards), the draft approval queue (grouped per source), scheduled
  // sends with Governor verdicts, and watch + delivery health. Draft/scheduled
  // ACTIONS go straight to the existing per-account draft endpoints via
  // cockpitApi.draftAction — the aggregate loads themselves never mutate.
  import { onMount } from 'svelte';
  import { Spinner, EmptyState, AgentCard, ApprovalRow, ScheduledRow, WatchPanel } from '$lib/components';
  import { EnvelopeApiError } from '$lib/api';
  import {
    cockpitApi,
    type AgentsResponse,
    type ScheduledResponse,
    type WatchesResponse,
    type ApprovalDraft,
    type ScheduledItem
  } from '$lib/cockpit-api';

  let agents = $state<AgentsResponse | null>(null);
  let scheduled = $state<ScheduledResponse | null>(null);
  let watches = $state<WatchesResponse | null>(null);
  let loading = $state(true);
  let error = $state<{ code: string; message: string } | null>(null);

  // Ids of drafts/scheduled currently running an action, so their rows disable.
  let busy = $state<Set<string>>(new Set());
  let actionError = $state<string | null>(null);

  async function load() {
    loading = true;
    error = null;
    try {
      const [a, s, w] = await Promise.all([
        cockpitApi.agents(),
        cockpitApi.scheduled(),
        cockpitApi.watches()
      ]);
      agents = a;
      scheduled = s;
      watches = w;
    } catch (e) {
      if (e instanceof EnvelopeApiError) {
        error = { code: e.code, message: e.message };
      } else {
        error = { code: 'unknown', message: e instanceof Error ? e.message : String(e) };
      }
    } finally {
      loading = false;
    }
  }

  onMount(load);

  function setBusy(id: string, on: boolean) {
    const next = new Set(busy);
    if (on) next.add(id);
    else next.delete(id);
    busy = next;
  }

  async function runDraftAction(
    actionBase: string,
    id: string,
    action: 'approve' | 'discard',
    body?: unknown
  ) {
    setBusy(id, true);
    actionError = null;
    try {
      await cockpitApi.draftAction(actionBase, action, body);
      await load();
    } catch (e) {
      actionError = e instanceof EnvelopeApiError ? e.message : String(e);
    } finally {
      setBusy(id, false);
    }
  }

  function onApprove(d: ApprovalDraft) {
    // Approval is bound to the revision the operator is looking at; the
    // server rejects (409) if the draft was edited since this view loaded.
    void runDraftAction(d.action_base, d.id, 'approve', {
      expected_revision: d.revision
    });
  }
  function onDiscard(d: ApprovalDraft) {
    void runDraftAction(d.action_base, d.id, 'discard');
  }
  function onEdit(d: ApprovalDraft) {
    // Editing a draft opens the per-account review surface (existing route). The
    // cockpit does not embed a composer; it hands off to the draft deep link.
    window.location.href = `/accounts/${encodeURIComponent(d.account_id)}/drafts/${encodeURIComponent(d.id)}`;
  }
  // Cancelling a scheduled send discards the queued draft via the existing
  // per-account draft discard endpoint.
  function onCancelScheduled(item: ScheduledItem) {
    void runDraftAction(item.action_base, item.id, 'discard');
  }

  // ── Formatting helpers ────────────────────────────────────────────────

  function age(iso: string | null): string {
    if (!iso) return 'never';
    const then = Date.parse(iso.includes('Z') || iso.includes('+') ? iso : `${iso}Z`);
    if (Number.isNaN(then)) return iso;
    const secs = Math.max(0, Math.floor((Date.now() - then) / 1000));
    if (secs < 60) return `${secs}s ago`;
    const mins = Math.floor(secs / 60);
    if (mins < 60) return `${mins}m ago`;
    const hrs = Math.floor(mins / 60);
    if (hrs < 24) return `${hrs}h ago`;
    return `${Math.floor(hrs / 24)}d ago`;
  }

  function countdown(item: ScheduledItem): string {
    if (item.due) return 'due now';
    const secs = item.seconds_remaining;
    if (secs == null) return 'scheduled';
    if (secs < 60) return `in ${secs}s`;
    const mins = Math.floor(secs / 60);
    if (mins < 60) return `in ${mins}m`;
    const hrs = Math.floor(mins / 60);
    if (hrs < 24) return `in ${hrs}h`;
    return `in ${Math.floor(hrs / 24)}d`;
  }
</script>

<div class="cockpit" id="cockpit">
  <header class="cockpit-header">
    <h1 class="cockpit-title">Agent Cockpit</h1>
    <p class="cockpit-lede">agent fleet on shared inbox — every action attributed.</p>
  </header>

  {#if loading}
    <div class="cockpit-loading"><Spinner /></div>
  {:else if error}
    <EmptyState
      title="Cockpit unavailable"
      hint={`${error.message} (${error.code})`}
    />
  {:else}
    {#if actionError}
      <p class="cockpit-action-error" role="alert">{actionError}</p>
    {/if}

    <div class="cockpit-grid">
      <!-- Per-agent attribution feed -->
      <section class="panel agents-panel" id="agents-panel">
        <div class="panel-head">
          <h2 class="panel-title">Agent fleet</h2>
          <span class="panel-count">{agents?.summary.active_agents ?? 0} active</span>
        </div>
        {#if !agents || agents.agents.length === 0}
          <EmptyState
            title="No agents registered"
            hint="Mint an agent identity with envelope agents add <name> to attribute actions."
          />
        {:else}
          <div class="agent-grid">
            {#each agents.agents as agent (agent.id)}
              <AgentCard {agent} {age} />
            {/each}
          </div>
        {/if}
      </section>

      <!-- Draft approval queue -->
      <section class="panel approval-panel" id="approval-panel">
        <div class="panel-head">
          <h2 class="panel-title">Drafts awaiting approval</h2>
          <span class="panel-count">{agents?.summary.awaiting_approval ?? 0}</span>
        </div>
        {#if !agents || agents.approval_queue.length === 0}
          <EmptyState
            title="Nothing awaiting approval"
            hint="Agent drafts land here for a human to approve, edit, or discard."
          />
        {:else}
          <p class="approval-note" id="approval-note">
            Approve records that you reviewed this version. It does not send the draft, and it does
            not exempt a later agent send from Governor — an agent that sends an approved draft is
            scored exactly as it would be otherwise. To send one yourself, open it and use
            <strong>Human-only Send</strong>.
          </p>
          {#each agents.approval_queue as group (group.source)}
            <div class="approval-group">
              <h3 class="approval-group-title">{group.source} · {group.count}</h3>
              <div class="approval-rows">
                {#each group.drafts as draft (draft.id)}
                  <ApprovalRow
                    {draft}
                    {age}
                    busy={busy.has(draft.id)}
                    onapprove={onApprove}
                    onedit={onEdit}
                    ondiscard={onDiscard}
                  />
                {/each}
              </div>
            </div>
          {/each}
        {/if}
      </section>

      <!-- Scheduled sends + Governor verdicts -->
      <section class="panel scheduled-panel" id="scheduled-panel">
        <div class="panel-head">
          <h2 class="panel-title">Scheduled sends</h2>
          <span class="panel-count">{scheduled?.summary.scheduled ?? 0}</span>
        </div>
        {#if !scheduled || scheduled.scheduled.length === 0}
          <EmptyState
            title="No scheduled sends"
            hint="Queue one with envelope send --at 'monday 9am'. Governor gates each at send time."
          />
        {:else}
          <div class="scheduled-rows">
            {#each scheduled.scheduled as item (item.id)}
              <ScheduledRow {item} {countdown} busy={busy.has(item.id)} oncancel={onCancelScheduled} />
            {/each}
          </div>
        {/if}
      </section>

      <!-- Watch + delivery health -->
      <section class="panel watches-panel" id="watches-panel">
        <div class="panel-head">
          <h2 class="panel-title">Watches &amp; deliveries</h2>
        </div>
        {#if watches}
          <WatchPanel data={watches} {age} />
        {/if}
      </section>

      <!-- Evidence -->
      <section class="panel evidence-panel" id="evidence-panel">
        <div class="panel-head">
          <h2 class="panel-title">Evidence bundles</h2>
        </div>
        <EmptyState
          title="Evidence lives on disk, not the server"
          hint="Evidence bundles are exported to a directory you choose. Run envelope evidence collect to build a verifiable .eml bundle with manifest + checksums."
        />
      </section>
    </div>
  {/if}
</div>

<style>
  .cockpit {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 1.5rem 2rem 3rem;
    width: 100%;
  }
  .cockpit-header {
    margin-bottom: 1.5rem;
  }
  .cockpit-title {
    margin: 0;
    font-family: var(--font-sans);
    font-size: 1.5rem;
    font-weight: 700;
    letter-spacing: -0.02em;
    color: var(--env-ink);
  }
  .cockpit-lede {
    margin: 0.25rem 0 0;
    font-size: 0.9375rem;
    color: var(--env-muted);
  }
  .cockpit-loading {
    display: flex;
    justify-content: center;
    padding: 4rem;
  }
  .cockpit-action-error {
    margin: 0 0 1rem;
    padding: 0.6rem 0.8rem;
    font-size: 0.8125rem;
    color: var(--env-warn);
    background: var(--env-warn-soft);
    border: 1px solid var(--env-warn);
    border-radius: var(--radius-sm, 3px);
  }
  .cockpit-grid {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 1.25rem;
    align-items: start;
  }
  .panel {
    background: var(--env-paper);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-md, 5px);
    padding: 1rem 1.1rem 1.25rem;
  }
  /* The agent fleet + watches span the full width; the rest sit two-up. */
  .agents-panel,
  .watches-panel {
    grid-column: 1 / -1;
  }
  .panel-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 0.5rem;
    margin-bottom: 0.85rem;
  }
  .panel-title {
    margin: 0;
    font-size: 1rem;
    font-weight: 600;
    color: var(--env-ink);
  }
  .panel-count {
    font-family: var(--font-mono);
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .agent-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
    gap: 1rem;
  }
  .approval-group + .approval-group {
    margin-top: 1rem;
  }
  .approval-note {
    margin: 0 0 0.75rem;
    font-size: 0.75rem;
    line-height: 1.5;
    color: var(--env-muted);
  }
  .approval-note strong {
    color: var(--env-ink);
  }
  .approval-group-title {
    margin: 0 0 0.5rem;
    font-family: var(--font-mono);
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--env-muted);
  }
  .approval-rows,
  .scheduled-rows {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  @media (max-width: 900px) {
    .cockpit-grid {
      grid-template-columns: 1fr;
    }
    .agents-panel,
    .watches-panel {
      grid-column: auto;
    }
  }
</style>
