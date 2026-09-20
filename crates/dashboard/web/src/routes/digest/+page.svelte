<script lang="ts">
  import { base } from '$app/paths';
  import {
    api,
    EnvelopeApiError,
    type MailEngineDecisionItem,
    type MailEngineDecisionsResponse,
    type MailEngineRoute
  } from '$lib/api';
  import { Button, EmptyState, Spinner } from '$lib/components';

  let response = $state<MailEngineDecisionsResponse | null>(null);
  let loading = $state(true);
  let refreshing = $state(false);
  let error = $state<{ code: string; message: string } | null>(null);
  let correctingKey = $state<string | null>(null);
  let correctionError = $state<string | null>(null);

  const routeOptions: Array<{ route: MailEngineRoute; label: string }> = [
    { route: 'follow_up', label: 'Needs reply' },
    { route: 'important', label: 'Important' },
    { route: 'digest_news', label: 'News digest' },
    { route: 'routine', label: 'Routine' },
    { route: 'junk', label: 'Junk' },
    { route: 'unsubscribe_candidate', label: 'Unsubscribe' },
    { route: 'review', label: 'Review' }
  ];

  const sectionCatalog = [
    { key: 'urgent', label: 'Urgent now', tone: 'urgent' },
    { key: 'review', label: 'Needs review', tone: 'review' },
    { key: 'follow_up', label: 'Needs reply', tone: 'reply' },
    { key: 'important', label: 'Important', tone: 'important' },
    { key: 'digest_news', label: 'News digest', tone: 'news' },
    { key: 'unsubscribe_candidate', label: 'Unsubscribe candidates', tone: 'unsubscribe' },
    { key: 'junk', label: 'Junk decisions', tone: 'junk' },
    { key: 'routine', label: 'Routine', tone: 'routine' }
  ] as const;

  function sectionKey(item: MailEngineDecisionItem): string {
    if (item.urgency === 'urgent' || item.urgency === 'critical') return 'urgent';
    if (item.status === 'review' || item.route === 'review') return 'review';
    return item.route;
  }

  const sections = $derived(
    sectionCatalog.map((section) => ({
      ...section,
      items: (response?.items ?? []).filter((item) => sectionKey(item) === section.key)
    }))
  );

  const reviewCount = $derived(
    (response?.items ?? []).filter((item) => item.status === 'review' || item.route === 'review').length
  );
  const urgentCount = $derived(
    (response?.items ?? []).filter(
      (item) => item.urgency === 'urgent' || item.urgency === 'critical'
    ).length
  );

  function fail(value: unknown) {
    const err = value as EnvelopeApiError;
    error = { code: err.code ?? 'unknown', message: err.message ?? 'Failed to load.' };
  }

  async function load() {
    loading = true;
    error = null;
    try {
      response = await api.mailEngineDecisions({ limit: 200 });
    } catch (value) {
      fail(value);
    } finally {
      loading = false;
    }
  }

  async function refresh() {
    refreshing = true;
    error = null;
    try {
      response = await api.mailEngineDecisions({ limit: 200 });
    } catch (value) {
      fail(value);
    } finally {
      refreshing = false;
    }
  }

  function fmtDate(value: string | null): string {
    if (!value) return 'date unavailable';
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) return value;
    return date.toLocaleString([], {
      month: 'short',
      day: 'numeric',
      hour: 'numeric',
      minute: '2-digit'
    });
  }

  function pct(value: number | null): string {
    return value === null ? 'confidence unavailable' : `${Math.round(value * 100)}% confidence`;
  }

  function remediation(errorCode: string): string {
    switch (errorCode) {
      case 'openrouter_api_key_missing':
        return 'Restore OPENROUTER_API_KEY, then use engine recover with --retry-jev --confirm-new-jev-call for this UID.';
      case 'jev_request_failed':
        return 'Check the OpenRouter key and connectivity. Review this row before authorizing another paid decision call.';
      case 'decision_incomplete':
        return 'Another pass may still be active. After ten minutes Envelope moves an abandoned claim into review.';
      case 'decision_interrupted':
        return 'Use Correct decision below to classify it locally, or inspect it before choosing an explicit recovery.';
      case 'notification_enqueue_failed':
        return 'Check the mail_engine_urgent event route and delivery storage. Envelope keeps the watermark held.';
      case 'credential_decrypt_failed':
        return 'Repair the Envelope credential store for this account.';
      case 'imap_connect_failed':
        return 'Check this account’s credentials and IMAP connectivity.';
      case 'imap_examine_failed':
        return 'Check that the configured folder still exists and is readable.';
      case 'highest_uid_unavailable':
        return 'Retry after the IMAP server returns a stable UID boundary.';
      case 'message_fetch_failed':
      case 'message_parse_failed':
        return 'Open the message directly and classify it with Correct decision below.';
      case 'spam_folder_not_found':
        return 'Configure or create the account’s Junk or Spam folder before applying junk actions.';
      case 'imap_move_failed':
        return 'The message was not moved. Check IMAP capabilities before retrying with --apply.';
      default:
        return 'Inspect the message and use Correct decision below, or check engine decisions in the CLI.';
    }
  }

  async function correct(item: MailEngineDecisionItem, route: MailEngineRoute) {
    const key = `${item.account_id}:${item.folder}:${item.uidvalidity}:${item.uid}`;
    correctingKey = key;
    correctionError = null;
    try {
      const urgency =
        item.urgency === 'urgent' || item.urgency === 'critical' ? item.urgency : 'not_urgent';
      await api.correctMailEngineDecision(item, { route, urgency });
      response = await api.mailEngineDecisions({ limit: 200 });
    } catch (value) {
      const err = value as EnvelopeApiError;
      correctionError = err.message ?? 'Correction failed. Refresh and try again.';
    } finally {
      correctingKey = null;
    }
  }

  $effect(() => {
    load();
  });
</script>

<svelte:head>
  <title>Mail engine — Envelope</title>
</svelte:head>

<div class="engine" id="mail-engine-cockpit">
  <header class="engine-head">
    <div>
      <h1>Mail engine</h1>
      <p>New mail classified by Jev. Message content below is untrusted mailbox data.</p>
    </div>
    <Button variant="ghost" onclick={refresh} disabled={loading || refreshing}>
      {refreshing ? 'Refreshing…' : 'Refresh'}
    </Button>
  </header>

  {#if loading}
    <div class="loading"><Spinner label="Loading mail engine" /> <span>Loading decisions…</span></div>
  {:else if error}
    <div class="error" role="alert">
      <strong>Mail-engine data could not be loaded.</strong>
      <p><code>{error.code}</code> {error.message}</p>
      <button type="button" onclick={load}>Retry</button>
    </div>
  {:else if response?.state === 'not_started'}
    <EmptyState
      title="Watching has not started"
      hint="Run `envelope engine once` to establish a new-mail-only baseline. Existing messages will not be sent to Jev."
    />
  {:else if response}
    <section class="status-strip" aria-label="Mail engine status">
      <div><b>{response.returned}</b><span>recent decisions</span></div>
      <div><b>{urgentCount}</b><span>urgent</span></div>
      <div><b>{reviewCount}</b><span>need review</span></div>
      <div><b>{response.pending_digest}</b><span>digest pending</span></div>
    </section>

    {#if response.urgent_notification.state === 'not_configured'}
      <p class="notice" role="status">
        Urgent decisions are visible here, but external alerts are not configured. Add a
        <code>mail_engine_urgent</code> event route and run the engine with <code>--deliver</code>.
      </p>
    {/if}

    {#if response.items.length === 0}
      <EmptyState
        title="Watching new mail"
        hint="The baseline is established. Decisions will appear after newer messages arrive."
      />
    {:else}
      {#each sections as section (section.key)}
        {#if section.items.length > 0}
          <section class="decision-section tone-{section.tone}" data-section={section.key}>
            <header>
              <h2>{section.label}</h2>
              <span>{section.items.length}</span>
            </header>
            <ul>
              {#each section.items as item (`${item.account_id}:${item.folder}:${item.uidvalidity}:${item.uid}`)}
                <li>
                  <a href="{base}{item.message_link}" class="decision-link">
                    <span class="from">
                      {item.untrusted_content.from ?? item.account_id}
                    </span>
                    <span class="subject">
                      {item.untrusted_content.subject ?? 'Message metadata unavailable'}
                    </span>
                  </a>
                  <div class="facts">
                    <span>{fmtDate(item.untrusted_content.date)}</span>
                    <span>{pct(item.route_confidence)}</span>
                    <span>{item.account_id}</span>
                    {#if item.execution_status !== 'not_requested'}
                      <span>action: {item.executed_action ?? item.execution_status}</span>
                    {/if}
                  </div>
                  {#if item.error_code}
                    <p class="problem">
                      <code>{item.error_code}</code> · {remediation(item.error_code)}
                    </p>
                  {/if}
                  {#if item.correction_revision > 0}
                    <p class="corrected">
                      Human correction r{item.correction_revision}; model said {item.model_route} / {item.model_urgency}.
                    </p>
                  {/if}
                  <details class="correction">
                    <summary>Correct decision</summary>
                    <div class="correction-options">
                      {#each routeOptions as option (option.route)}
                        <button
                          type="button"
                          disabled={item.route === option.route || correctingKey === `${item.account_id}:${item.folder}:${item.uidvalidity}:${item.uid}`}
                          onclick={() => correct(item, option.route)}
                        >
                          {option.label}
                        </button>
                      {/each}
                    </div>
                  </details>
                </li>
              {/each}
            </ul>
          </section>
        {/if}
      {/each}
    {/if}
    {#if correctionError}
      <p class="problem" role="alert">{correctionError}</p>
    {/if}
  {/if}
</div>

<style>
  .engine {
    max-width: 58rem;
    height: 100%;
    margin: 0 auto;
    padding: 1.25rem 1.25rem 3rem;
    overflow-y: auto;
  }

  .engine-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 1rem;
  }

  h1 {
    margin: 0;
    font-size: 1.45rem;
  }

  .engine-head p {
    margin: 0.3rem 0 0;
    color: var(--env-muted);
    font-size: 0.85rem;
  }

  .status-strip {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    gap: 0.5rem;
    margin: 1rem 0 1.25rem;
  }

  .status-strip div {
    border: 1px solid var(--env-rule);
    border-radius: 0.5rem;
    padding: 0.65rem;
    background: var(--env-surface);
  }

  .status-strip b,
  .status-strip span {
    display: block;
  }

  .status-strip b {
    font-size: 1.15rem;
  }

  .status-strip span {
    margin-top: 0.15rem;
    color: var(--env-muted);
    font-size: 0.75rem;
  }

  .notice {
    margin: -0.5rem 0 1rem;
    padding: 0.65rem 0.75rem;
    border: 1px solid var(--env-warn);
    border-radius: 0.45rem;
    color: var(--env-ink);
    font-size: 0.8rem;
  }

  .decision-section {
    margin-top: 1.25rem;
    border-left: 3px solid var(--env-rule);
    padding-left: 0.75rem;
  }

  .tone-urgent,
  .tone-review {
    border-left-color: var(--env-danger, #b42318);
  }

  .tone-reply,
  .tone-important {
    border-left-color: var(--env-warn);
  }

  .tone-news {
    border-left-color: var(--env-accent);
  }

  .decision-section > header {
    display: flex;
    align-items: baseline;
    gap: 0.55rem;
  }

  .decision-section h2 {
    margin: 0;
    font-size: 1rem;
  }

  .decision-section header span {
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.75rem;
  }

  ul {
    list-style: none;
    margin: 0.5rem 0 0;
    padding: 0;
  }

  li {
    padding: 0.7rem 0;
    border-bottom: 1px solid var(--env-rule);
  }

  .decision-link {
    display: grid;
    grid-template-columns: minmax(9rem, 0.35fr) minmax(0, 1fr);
    gap: 0.6rem;
    color: var(--env-ink);
    text-decoration: none;
  }

  .decision-link:hover .subject {
    text-decoration: underline;
  }

  .from {
    font-weight: 650;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .subject {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .facts {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem 0.75rem;
    margin-top: 0.3rem;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.7rem;
  }

  .problem {
    margin: 0.35rem 0 0;
    color: var(--env-danger, #b42318);
    font-size: 0.78rem;
  }

  .corrected {
    margin: 0.35rem 0 0;
    color: var(--env-accent);
    font-size: 0.75rem;
  }

  .correction {
    margin-top: 0.45rem;
    font-size: 0.75rem;
  }

  .correction summary {
    cursor: pointer;
    color: var(--env-accent);
  }

  .correction-options {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
    margin-top: 0.45rem;
  }

  .correction-options button {
    border: 1px solid var(--env-rule);
    border-radius: 0.35rem;
    padding: 0.3rem 0.45rem;
    background: var(--env-surface);
    color: var(--env-ink);
    cursor: pointer;
  }

  .correction-options button:disabled {
    opacity: 0.45;
    cursor: default;
  }

  .loading,
  .error {
    margin-top: 1.5rem;
  }

  .error p {
    margin: 0.4rem 0;
  }

  @media (max-width: 640px) {
    .engine {
      padding: 0.9rem 0.85rem 2rem;
    }

    .status-strip {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }

    .decision-link {
      display: block;
    }

    .from,
    .subject {
      display: block;
      white-space: normal;
    }

    .subject {
      margin-top: 0.2rem;
    }
  }
</style>
