// AgentCard — read-only agent identity card, shown on Rules.
import { render, screen } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import AgentCard from './AgentCard.svelte';
import type { AgentCard as AgentCardT } from '$lib/agents-api';

const age = (iso: string | null) => (iso ? 'recently' : 'never');

const agentFixture: AgentCardT = {
  id: 'agent-1',
  name: 'skippy',
  token_prefix: 'envtok_1a2b3c4d',
  created_at: '2026-07-01T00:00:00Z',
  revoked_at: null,
  last_used_at: '2026-07-08T00:00:00Z',
  status: 'active',
  activity: { action_count: 3, event_count: 4, last_activity_at: '2026-07-08T00:00:00Z' },
  policy: {
    send_mode_ceiling: 'draft-only',
    accounts: 'all',
    folders: 'all',
    actions: 'all',
    recipients: 'restricted'
  }
};

describe('AgentCard', () => {
  it('renders name, token prefix, total activity, and send ceiling', () => {
    render(AgentCard, { agent: agentFixture, age });
    expect(screen.getByText('skippy')).toBeTruthy();
    expect(screen.getByText('envtok_1a2b3c4d')).toBeTruthy();
    // action_count + event_count = 7
    expect(screen.getByText('7')).toBeTruthy();
    expect(screen.getByText('draft-only')).toBeTruthy();
  });
});
