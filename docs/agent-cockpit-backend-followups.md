# Agent Cockpit backend follow-ups

The Agent Cockpit dashboard surface is wired to durable operational primitives without doing live auth probes or mutating mailboxes from aggregate loads.

## Implemented in backend pass

1. Persistent watch registry
   - `watch_registry` stores watch status by account/folder, process id, schedule, heartbeat/event timestamps, and failure reason.
   - Cockpit now returns `watches.status = available` and concrete `watches.items`.

2. Failed auth history
   - `failed_auth_history` stores account id, backend, redacted reason, retry guidance, and timestamp.
   - Account verification failures append IMAP auth history; Cockpit reads history only and still does not run live credential checks.

3. Draft operator actions
   - Dashboard API endpoints now exist for agent-created drafts:
     - `POST /api/accounts/{id}/drafts/{draft_id}/approve`
     - `POST /api/accounts/{id}/drafts/{draft_id}/edit`
     - `POST /api/accounts/{id}/drafts/{draft_id}/discard`
     - `POST /api/accounts/{id}/drafts/{draft_id}/block`
     - `POST /api/accounts/{id}/drafts/{draft_id}/send` with `confirm: true`
   - Draft actions are account-scoped and use store primitives; send remains an explicit mutating endpoint, never part of Cockpit aggregate load.

4. Rule run audit records
   - `rule_run_audit` stores normalized rule execution records: account, rule id/name, UID, folder, action, status, error, and timestamp.
   - Cockpit reads recent rule runs from the normalized audit table instead of inferring from action log text.

## Invariants preserved

- No live credential probing from Cockpit aggregate load.
- No mailbox mutation from Cockpit aggregate endpoint.
- Rules Control Plane behavior remains separate from Cockpit aggregation.
