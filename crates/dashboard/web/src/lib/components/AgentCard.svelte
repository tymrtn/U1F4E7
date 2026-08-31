<script lang="ts">
  // Agent identity card — the cockpit money shot. Name prominent, envtok_ prefix
  // in a MonoTag, last-active + attributed action/event counts, and a compact
  // policy summary (send-mode ceiling + scope tightness). Read-only.
  import MonoTag from './MonoTag.svelte';
  import type { AgentCard } from '$lib/cockpit-api';

  let { agent, age }: { agent: AgentCard; age: (iso: string | null) => string } = $props();

  const totalActivity = $derived(agent.activity.action_count + agent.activity.event_count);
  const revoked = $derived(agent.status === 'revoked');
</script>

<article class="agent-card" class:revoked>
  <header class="agent-head">
    <div class="agent-id">
      <h3 class="agent-name">{agent.name}</h3>
      <MonoTag>{agent.token_prefix}</MonoTag>
    </div>
    <span class="agent-status" class:is-revoked={revoked}>
      <span class="status-light" aria-hidden="true"></span>
      <span class="status-label">{revoked ? 'revoked' : 'active'}</span>
    </span>
  </header>

  <dl class="agent-stats">
    <div class="stat">
      <dt>Actions</dt>
      <dd class="stat-num">{totalActivity}</dd>
    </div>
    <div class="stat">
      <dt>Last active</dt>
      <dd>{age(agent.activity.last_activity_at ?? agent.last_used_at)}</dd>
    </div>
    <div class="stat">
      <dt>Send ceiling</dt>
      <dd><MonoTag>{agent.policy.send_mode_ceiling}</MonoTag></dd>
    </div>
  </dl>

  <footer class="agent-policy">
    <span class="policy-scope" class:tight={agent.policy.accounts === 'restricted'}>
      accounts: {agent.policy.accounts}
    </span>
    <span class="policy-scope" class:tight={agent.policy.actions === 'restricted'}>
      actions: {agent.policy.actions}
    </span>
    <span class="policy-scope" class:tight={agent.policy.recipients === 'restricted'}>
      recipients: {agent.policy.recipients}
    </span>
  </footer>
</article>

<style>
  .agent-card {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    padding: 1rem;
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-md, 5px);
  }
  .agent-card.revoked {
    opacity: 0.6;
  }
  .agent-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 0.5rem;
  }
  .agent-id {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    min-width: 0;
  }
  .agent-name {
    margin: 0;
    font-family: var(--font-sans);
    font-size: 1.05rem;
    font-weight: 600;
    letter-spacing: -0.01em;
    color: var(--env-ink);
    overflow: hidden;
    text-overflow: ellipsis;
  }
  /* Instrument status light: a lit dot + mono readout (design plan rev 3, D). */
  .agent-status {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    flex-shrink: 0;
  }
  .status-light {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--env-accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--env-accent) 20%, transparent);
  }
  .agent-status.is-revoked .status-light {
    background: var(--env-warn);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--env-warn) 20%, transparent);
  }
  .status-label {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-accent);
  }
  .agent-status.is-revoked .status-label {
    color: var(--env-warn);
  }
  .agent-stats {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 0.5rem;
    margin: 0;
  }
  .stat {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
  }
  .stat dt {
    font-size: 0.6875rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--env-muted);
  }
  .stat dd {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-ink);
  }
  .stat-num {
    font-family: var(--font-mono);
    font-size: 1.15rem;
    font-weight: 600;
  }
  .agent-policy {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem 0.75rem;
    padding-top: 0.5rem;
    border-top: 1px solid var(--env-rule);
  }
  .policy-scope {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
  .policy-scope.tight {
    color: var(--env-accent);
  }
</style>
