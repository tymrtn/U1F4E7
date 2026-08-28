<script lang="ts">
  // Review — the operator's daily queue. One scannable page of what is
  // pending, grouped by the decision needed, across every account. Strictly
  // read-only: every row deep-links to the surface where the action happens
  // (draft review, message reader, rules page). The Cockpit stays the
  // diagnostic view; this page is the queue.
  import { onMount } from 'svelte';
  import { base } from '$app/paths';
  import { Spinner, EmptyState } from '$lib/components';
  import { EnvelopeApiError } from '$lib/api';
  import { reviewApi, type ReviewResponse } from '$lib/review-api';

  let data = $state<ReviewResponse | null>(null);
  let loading = $state(true);
  let error = $state<{ code: string; message: string } | null>(null);

  async function load() {
    loading = true;
    error = null;
    try {
      data = await reviewApi.get();
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

  function age(iso: string | null): string {
    if (!iso) return '';
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

  function countdown(due: boolean, secondsRemaining: number | null): string {
    if (due) return 'due now';
    if (secondsRemaining == null) return 'scheduled';
    if (secondsRemaining < 60) return `in ${secondsRemaining}s`;
    const mins = Math.floor(secondsRemaining / 60);
    if (mins < 60) return `in ${mins}m`;
    const hrs = Math.floor(mins / 60);
    if (hrs < 24) return `in ${hrs}h`;
    return `in ${Math.floor(hrs / 24)}d`;
  }

  function draftState(status: string): string {
    return status === 'pending_review' ? 'pending review' : status;
  }
</script>

<div class="review" id="review">
  <header class="review-header">
    <h1 class="review-title">Review</h1>
    <p class="review-lede">Pending decisions across all accounts.</p>
  </header>

  {#if loading}
    <div class="review-loading"><Spinner /></div>
  {:else if error}
    <EmptyState title="Review unavailable" hint={`${error.message} (${error.code})`} />
  {:else if data}
    <!-- 1 · Decide now -->
    <section class="group" id="review-decide-now">
      <div class="group-head">
        <h2 class="group-title">Decide now</h2>
        <span class="group-count">{data.summary.decide_now}</span>
      </div>
      {#if data.decide_now.count === 0}
        <EmptyState
          title="Nothing to decide"
          hint="Pending drafts, failed actions, and rule proposals land here."
        />
      {:else}
        {#if data.decide_now.drafts.items.length > 0}
          <h3 class="sub-title">
            Drafts awaiting review · {data.decide_now.drafts.items.length}
          </h3>
          <ul class="rows">
            {#each data.decide_now.drafts.items as draft (draft.id)}
              <li class="row">
                <a class="row-link" href="{base}{draft.link}">
                  <span class="row-main">{draft.subject ?? '(no subject)'}</span>
                  <span class="row-meta">
                    {draft.account_label}
                    {#if draft.created_by}· from {draft.created_by}{/if}
                    · {age(draft.updated_at)}
                  </span>
                </a>
                <span class="chip" class:chip-danger={draft.status === 'blocked'}>
                  {draftState(draft.status)}
                </span>
              </li>
            {/each}
          </ul>
        {/if}
        {#if data.decide_now.failed_actions.items.length > 0}
          <h3 class="sub-title">
            Failed agent actions · {data.decide_now.failed_actions.items.length}
          </h3>
          <ul class="rows">
            {#each data.decide_now.failed_actions.items as action (action.id)}
              <li class="row">
                {#if action.draft_link}
                  <a class="row-link" href="{base}{action.draft_link}">
                    <span class="row-main">{action.action_type}: {action.justification}</span>
                    <span class="row-meta">{action.account_label} · {age(action.created_at)}</span>
                  </a>
                {:else}
                  <div class="row-link">
                    <span class="row-main">{action.action_type}: {action.justification}</span>
                    <span class="row-meta">{action.account_label} · {age(action.created_at)}</span>
                  </div>
                {/if}
                <span class="chip chip-danger">{action.action_status}</span>
              </li>
            {/each}
          </ul>
        {/if}
        {#if data.decide_now.events.items.length > 0}
          <h3 class="sub-title">Unacknowledged events · {data.decide_now.events.items.length}</h3>
          <ul class="rows">
            {#each data.decide_now.events.items as event (event.id)}
              <li class="row">
                <div class="row-link">
                  <span class="row-main">{event.subject ?? event.event_type}</span>
                  <span class="row-meta">
                    {event.account_label} · {event.event_type} · {age(event.created_at)}
                  </span>
                </div>
                <span class="chip">{event.outcome}</span>
              </li>
            {/each}
          </ul>
        {/if}
        {#if data.decide_now.proposed_rules.items.length > 0}
          <h3 class="sub-title">
            Proposed rules · {data.decide_now.proposed_rules.items.length}
          </h3>
          <p class="group-note">
            Proposals do nothing until you enable them on the Rules page.
          </p>
          <ul class="rows">
            {#each data.decide_now.proposed_rules.items as rule (rule.id)}
              <li class="row">
                <a class="row-link" href="{base}{rule.link}">
                  <span class="row-main">{rule.name}</span>
                  <span class="row-meta">{rule.account_label}</span>
                </a>
                <span class="chip">proposal · disabled</span>
              </li>
            {/each}
          </ul>
        {/if}
      {/if}
    </section>

    <!-- 2 · Waiting / scheduled -->
    <section class="group" id="review-waiting">
      <div class="group-head">
        <h2 class="group-title">Waiting</h2>
        <span class="group-count">{data.summary.waiting}</span>
      </div>
      {#if data.waiting.count === 0}
        <EmptyState
          title="Nothing waiting"
          hint="Scheduled sends and snoozes appear here with their due times."
        />
      {:else}
        {#if data.waiting.scheduled.items.length > 0}
          <h3 class="sub-title">
            Scheduled sends · {data.waiting.scheduled.items.length}
          </h3>
          <ul class="rows">
            {#each data.waiting.scheduled.items as item (item.id)}
              <li class="row">
                <a class="row-link" href="{base}{item.link}">
                  <span class="row-main">{item.subject ?? '(no subject)'}</span>
                  <span class="row-meta">
                    {item.account_label}
                    {#if item.created_by}· from {item.created_by}{/if}
                  </span>
                </a>
                <span class="chip" class:chip-pending={item.due}>
                  {countdown(item.due, item.seconds_remaining)}
                </span>
              </li>
            {/each}
          </ul>
        {/if}
        {#if data.waiting.due_snoozes.items.length > 0}
          <h3 class="sub-title">Due snoozes · {data.waiting.due_snoozes.items.length}</h3>
          <ul class="rows">
            {#each data.waiting.due_snoozes.items as snooze (snooze.id)}
              <li class="row">
                <a class="row-link" href="{base}{snooze.message_link}">
                  <span class="row-main">{snooze.subject ?? '(no subject)'}</span>
                  <span class="row-meta">
                    {snooze.account_label}
                    {#if snooze.reason}· {snooze.reason}{/if}
                  </span>
                </a>
                <span class="chip chip-pending">due now</span>
              </li>
            {/each}
          </ul>
        {/if}
        {#if data.waiting.awaiting_reply.items.length > 0}
          <h3 class="sub-title">
            Awaiting reply · {data.waiting.awaiting_reply.items.length}
          </h3>
          <ul class="rows">
            {#each data.waiting.awaiting_reply.items as snooze (snooze.id)}
              <li class="row">
                <a class="row-link" href="{base}{snooze.message_link}">
                  <span class="row-main">{snooze.subject ?? '(no subject)'}</span>
                  <span class="row-meta">
                    {snooze.account_label}
                    {#if snooze.note}· {snooze.note}{/if}
                  </span>
                </a>
                <span class="chip">returns {age(snooze.return_at) || snooze.return_at}</span>
              </li>
            {/each}
          </ul>
        {/if}
      {/if}
    </section>

    <!-- 3 · Needs triage -->
    <section class="group" id="review-needs-triage">
      <div class="group-head">
        <h2 class="group-title">Needs triage</h2>
        <span class="group-count">{data.summary.needs_triage}</span>
      </div>
      {#if data.needs_triage.count === 0}
        <EmptyState
          title="No flagged messages"
          hint="Only durable mailbox events land here — Envelope does not scan or classify your inbox."
        />
      {:else}
        <p class="group-note">
          Messages flagged by watches and rules. Envelope does not scan or classify your inbox.
        </p>
        <ul class="rows">
          {#each data.needs_triage.items as item (item.id)}
            <li class="row">
              {#if item.message_link}
                <a class="row-link" href="{base}{item.message_link}">
                  <span class="row-main">{item.subject ?? item.event_type}</span>
                  <span class="row-meta">
                    {item.account_label}
                    {#if item.from_addr}· {item.from_addr}{/if}
                    · {age(item.created_at)}
                  </span>
                </a>
              {:else}
                <div class="row-link">
                  <span class="row-main">{item.subject ?? item.event_type}</span>
                  <span class="row-meta">{item.account_label} · {age(item.created_at)}</span>
                </div>
              {/if}
              <span class="chip" class:chip-pending={item.secure_pending}>
                {item.secure_pending ? 'secure' : item.outcome}
              </span>
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <!-- 4 · Operational health -->
    <section class="group" id="review-operational-health">
      <div class="group-head">
        <h2 class="group-title">Operational health</h2>
        <span class="group-count">{data.summary.operational_health}</span>
      </div>
      {#if data.operational_health.count === 0}
        <EmptyState
          title="All clear"
          hint="Failed logins, failed watches, and dead-lettered deliveries appear here."
        />
      {:else}
        {#if data.operational_health.failed_auth.items.length > 0}
          <h3 class="sub-title">
            Failed auth · {data.operational_health.failed_auth.items.length}
          </h3>
          <ul class="rows">
            {#each data.operational_health.failed_auth.items as attempt (attempt.id)}
              <li class="row">
                <div class="row-link">
                  <span class="row-main">{attempt.account_label}: {attempt.backend} login failed</span>
                  <span class="row-meta">
                    {attempt.retry_guidance ?? attempt.reason} · {age(attempt.created_at)}
                  </span>
                </div>
                <span class="chip chip-danger">auth</span>
              </li>
            {/each}
          </ul>
        {/if}
        {#if data.operational_health.failed_watches.items.length > 0}
          <h3 class="sub-title">
            Failed watches · {data.operational_health.failed_watches.items.length}
          </h3>
          <ul class="rows">
            {#each data.operational_health.failed_watches.items as watch (watch.id)}
              <li class="row">
                <div class="row-link">
                  <span class="row-main">{watch.account_label} · {watch.folder}</span>
                  <span class="row-meta">{watch.failure_reason ?? watch.status}</span>
                </div>
                <span class="chip chip-danger">{watch.status}</span>
              </li>
            {/each}
          </ul>
        {/if}
        {#if data.operational_health.dead_letters.routes.length > 0}
          <h3 class="sub-title">
            Dead-lettered deliveries · {data.operational_health.dead_letters.count}
          </h3>
          <ul class="rows">
            {#each data.operational_health.dead_letters.routes as route (route.id)}
              <li class="row">
                <div class="row-link">
                  <span class="row-main">{route.account_label} route</span>
                  <span class="row-meta">{route.dead} dead deliveries</span>
                </div>
                <span class="chip chip-danger">dead letter</span>
              </li>
            {/each}
          </ul>
        {/if}
      {/if}
    </section>
  {/if}
</div>

<style>
  /* Phone-first: a single column of stacked groups; wider screens get
     breathing room, not a rearrangement — the priority order IS the layout. */
  .review {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 1rem 1rem 3rem;
    width: 100%;
    max-width: 44rem;
    margin: 0 auto;
  }
  .review-header {
    margin-bottom: 1rem;
  }
  .review-title {
    margin: 0;
    font-family: var(--font-sans);
    font-size: 1.5rem;
    font-weight: 700;
    letter-spacing: -0.02em;
    color: var(--env-ink);
  }
  .review-lede {
    margin: 0.25rem 0 0;
    font-size: 0.9375rem;
    color: var(--env-muted);
  }
  .review-loading {
    display: flex;
    justify-content: center;
    padding: 4rem;
  }
  .group {
    background: var(--env-paper);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-md, 5px);
    padding: 0.85rem 1rem 1rem;
    margin-bottom: 1rem;
  }
  .group-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 0.5rem;
    margin-bottom: 0.65rem;
  }
  .group-title {
    margin: 0;
    font-size: 1rem;
    font-weight: 600;
    color: var(--env-ink);
  }
  .group-count {
    font-family: var(--font-mono);
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .group-note {
    margin: 0 0 0.6rem;
    font-size: 0.75rem;
    line-height: 1.5;
    color: var(--env-muted);
  }
  .sub-title {
    margin: 0.85rem 0 0.4rem;
    font-family: var(--font-mono);
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--env-muted);
  }
  .sub-title:first-of-type {
    margin-top: 0;
  }
  .rows {
    margin: 0;
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.6rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    padding: 0.45rem 0.6rem;
  }
  .row-link {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    min-width: 0;
    text-decoration: none;
    color: inherit;
  }
  a.row-link:hover .row-main {
    color: var(--env-accent);
  }
  .row-main {
    font-size: 0.8125rem;
    font-weight: 500;
    color: var(--env-ink);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .row-meta {
    font-size: 0.71875rem;
    color: var(--env-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .chip {
    flex-shrink: 0;
    font-family: var(--font-mono);
    font-size: 0.65625rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--env-muted);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    padding: 0.15rem 0.4rem;
    white-space: nowrap;
  }
  .chip-danger {
    color: var(--env-warn);
    border-color: var(--env-warn);
  }
  .chip-pending {
    color: var(--env-pending);
    border-color: var(--env-pending);
  }
  @media (min-width: 641px) {
    .review {
      padding: 1.5rem 2rem 3rem;
    }
  }
</style>
