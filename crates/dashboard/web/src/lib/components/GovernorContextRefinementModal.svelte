<script lang="ts">
  import Button from './Button.svelte';
  import Modal from './Modal.svelte';
  import MonoTag from './MonoTag.svelte';
  import Spinner from './Spinner.svelte';
  import {
    api,
    type ContextRefinementResponse,
    type ContextRefinementRetryResponse,
    type EnvelopeApiError
  } from '$lib/api';

  let {
    open = false,
    accountId,
    draftId,
    onclose,
    onsuccess
  }: {
    open?: boolean;
    accountId: string;
    draftId: string;
    onclose: () => void;
    onsuccess: (result: ContextRefinementRetryResponse) => void;
  } = $props();

  let loading = $state(false);
  let submitting = $state(false);
  let refinement = $state<ContextRefinementResponse | null>(null);
  let selected = $state<string[]>([]);
  let confirmed = $state(false);
  let error = $state('');
  let conflict = $state(false);
  let loadedKey = '';
  let generation = 0;

  const declarable = $derived(
    refinement?.attributes.filter((attribute) => attribute.provenance === 'declarable') ?? []
  );
  const hostFacts = $derived(
    refinement?.attributes.filter((attribute) => attribute.provenance === 'host_derived') ?? []
  );
  const authorityFacts = $derived(
    refinement?.attributes.filter((attribute) => attribute.provenance === 'requires_attestation') ?? []
  );
  const canRetry = $derived(
    !!refinement?.eligible && selected.length > 0 && confirmed && !submitting && !conflict
  );

  $effect(() => {
    const key = open ? `${accountId}:${draftId}` : '';
    if (!key || key === loadedKey) return;
    loadedKey = key;
    void load();
  });

  $effect(() => {
    if (open) return;
    generation += 1;
    loadedKey = '';
    loading = false;
    submitting = false;
    refinement = null;
    selected = [];
    confirmed = false;
    error = '';
    conflict = false;
  });

  async function load() {
    const requestGeneration = ++generation;
    loading = true;
    refinement = null;
    selected = [];
    confirmed = false;
    error = '';
    conflict = false;
    try {
      const result = await api.contextRefinement(accountId, draftId);
      if (requestGeneration !== generation) return;
      refinement = result;
      selected = result.attributes
        .filter((attribute) => attribute.provenance === 'declarable' && attribute.state === 'selected')
        .map((attribute) => attribute.key);
      if (!result.eligible) error = result.explanation;
    } catch (cause) {
      if (requestGeneration !== generation) return;
      const apiError = cause as EnvelopeApiError;
      error = apiError.message ?? 'Could not load the factual context for this draft.';
    } finally {
      if (requestGeneration === generation) loading = false;
    }
  }

  function close() {
    if (!submitting) onclose();
  }

  function toggle(key: string, checked: boolean) {
    if (checked && !selected.includes(key)) selected = [...selected, key];
    if (!checked) selected = selected.filter((candidate) => candidate !== key);
  }

  async function retry() {
    if (!refinement || !canRetry) return;
    submitting = true;
    error = '';
    try {
      const result = await api.retryContextRefinement(accountId, draftId, {
        expected_revision: refinement.revision,
        declarable_attributes: selected,
        confirm_factual_accuracy: true
      });
      onsuccess(result);
    } catch (cause) {
      const apiError = cause as EnvelopeApiError;
      if (apiError.status === 409) {
        conflict = true;
        error =
          'This draft changed or is no longer eligible. Nothing was queued. Close this dialog, reload the draft, and review its context again.';
      } else {
        error = apiError.message ?? 'Could not record the corrected context.';
      }
    } finally {
      submitting = false;
    }
  }
</script>

<Modal {open} title="Refine Governor context" onclose={close}>
  {#if loading}
    <div class="context-loading"><Spinner label="Loading context" /> Loading factual context…</div>
  {:else if refinement}
    <p class="context-intro">{refinement.explanation}</p>
    {#if refinement.reason_code}<MonoTag>{refinement.reason_code}</MonoTag>{/if}

    <section class="context-section" aria-labelledby="context-declarable-heading">
      <h3 id="context-declarable-heading">Facts you can correct</h3>
      <p>Select only facts that are true of this exact draft. This replaces the prior declaration.</p>
      <div class="context-list">
        {#each declarable as attribute (attribute.key)}
          <label class="context-choice">
            <input
              type="checkbox"
              checked={selected.includes(attribute.key)}
              disabled={!refinement.eligible || submitting || conflict}
              onchange={(event) => toggle(attribute.key, event.currentTarget.checked)}
            />
            <span><strong>{attribute.description}</strong><MonoTag>{attribute.key}</MonoTag></span>
          </label>
        {/each}
      </div>
    </section>

    <section class="context-section" aria-labelledby="context-host-heading">
      <h3 id="context-host-heading">Observed by Envelope</h3>
      <p>These host facts are read-only and are re-derived from the stored draft.</p>
      <div class="context-list">
        {#each hostFacts as attribute (attribute.key)}
          <div class="context-fact">
            <span><strong>{attribute.description}</strong><MonoTag>{attribute.key}</MonoTag></span>
            <span class="context-state">{attribute.state.replaceAll('_', ' ')}</span>
          </div>
        {/each}
      </div>
    </section>

    {#if authorityFacts.length > 0}
      <section class="context-section" aria-labelledby="context-authority-heading">
        <h3 id="context-authority-heading">Authority facts</h3>
        <p>Authority facts are unavailable here. This correction is not approval or Human-only Send.</p>
        <div class="context-list">
          {#each authorityFacts as attribute (attribute.key)}
            <div class="context-fact is-unavailable">
              <span><strong>{attribute.description}</strong><MonoTag>{attribute.key}</MonoTag></span>
              <span class="context-state">Unavailable</span>
            </div>
          {/each}
        </div>
      </section>
    {/if}

    <label class="context-confirm">
      <input
        type="checkbox"
        bind:checked={confirmed}
        disabled={!refinement.eligible || submitting || conflict}
      />
      I confirm these factual labels are accurate for revision {refinement.revision}.
    </label>
    <p class="context-consequence">
      Retry records this correction and requeues the draft. The normal Governor check still decides
      whether any transmission may occur.
    </p>
  {/if}

  {#if error}
    <p class:context-conflict={conflict} class="context-error" role="alert">{error}</p>
  {/if}
  {#if submitting}
    <p class="context-locked">Recording this correction. This dialog cannot be closed until it finishes.</p>
  {/if}

  {#snippet footer()}
    <Button variant="ghost" disabled={submitting} onclick={close}>Cancel</Button>
    <Button variant="primary" disabled={!canRetry} onclick={retry}>
      {#if submitting}<Spinner label="Retrying" />{/if}
      Retry with corrected context
    </Button>
  {/snippet}
</Modal>

<style>
  .context-loading,
  .context-choice,
  .context-fact {
    display: flex;
    align-items: center;
    gap: 0.65rem;
  }
  .context-intro,
  .context-section p,
  .context-consequence,
  .context-error,
  .context-locked {
    margin: 0.35rem 0;
    color: var(--env-muted);
  }
  .context-section {
    margin-top: 1rem;
  }
  .context-section h3 {
    margin: 0;
    color: var(--env-ink);
    font-size: 0.8125rem;
  }
  .context-list {
    display: grid;
    gap: 0.4rem;
    margin-top: 0.5rem;
  }
  .context-choice,
  .context-fact {
    justify-content: space-between;
    padding: 0.45rem 0.55rem;
    border: 1px solid var(--env-rule);
    background: var(--env-page);
  }
  .context-choice {
    justify-content: flex-start;
    cursor: pointer;
  }
  .context-choice span,
  .context-fact > span:first-child {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.4rem;
  }
  .context-choice strong,
  .context-fact strong {
    color: var(--env-ink);
    font-weight: 500;
  }
  .context-state {
    flex: none;
    color: var(--env-muted);
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    text-transform: capitalize;
  }
  .is-unavailable {
    opacity: 0.7;
  }
  .context-confirm {
    display: flex;
    align-items: flex-start;
    gap: 0.55rem;
    margin-top: 1rem;
    color: var(--env-ink);
  }
  .context-confirm input {
    margin-top: 0.2rem;
  }
  .context-error,
  .context-locked {
    color: var(--env-warn);
  }
  .context-conflict {
    font-weight: 600;
  }
</style>
