<script lang="ts">
  // /v2/rules — Rules control plane for the Envelope v2 dashboard.
  //
  // Deliverables:
  //   • Per-account rule list with account switcher
  //   • Each rule: name, human-readable match + action summary, enabled toggle
  //     (with confirm when enabling a rule that deletes/unsubscribes)
  //   • Create / edit modal: matcher builder + action builder
  //   • Hit stats (hit_count + last_hit_at) when present
  //
  // Safety bounds from CLAUDE.md:
  //   "Preserve the Rules Control Plane controls and safety bounds when adding
  //    dashboard surfaces."  That means: no silent destructive actions, all
  //    enable/disable/delete go through explicit confirm, webhook URLs are
  //    never shown (server redacts them), and the enable-toggle requires extra
  //    confirmation for delete/unsubscribe rules.

  import { onMount } from 'svelte';
  import { Modal, Button, Spinner, EmptyState, MonoTag, Toast } from '$lib/components';
  import { api, type Account, type FolderStats, EnvelopeApiError } from '$lib/api';
  import {
    rulesApi,
    buildMatchExpr,
    buildAction,
    parseMatchExpr,
    parseAction,
    matchSummary,
    actionSummary,
    isHighRiskAction,
    type Rule,
    type MatchFields,
    type ActionFields,
    type ActionKind
  } from '$lib/rules-api';

  // ── Page state ────────────────────────────────────────────────────────

  let accounts = $state<Account[]>([]);
  let selectedAccountId = $state<string | null>(null);
  let rules = $state<Rule[]>([]);
  let folders = $state<FolderStats[]>([]);

  let loadingAccounts = $state(true);
  let loadingRules = $state(false);
  let pageError = $state<string | null>(null);
  let rulesError = $state<string | null>(null);

  // Toast notifications
  let toastMsg = $state<string | null>(null);
  let toastVariant = $state<'ok' | 'danger'>('ok');

  function showToast(msg: string, kind: 'ok' | 'error' = 'ok') {
    toastMsg = msg;
    toastVariant = kind === 'error' ? 'danger' : 'ok';
  }

  function dismissToast() {
    toastMsg = null;
  }

  // Pending state per rule-id for toggle operations.
  let pendingToggle = $state(new Set<string>());

  // ── Editor modal state ────────────────────────────────────────────────

  let editorOpen = $state(false);
  let editorMode = $state<'create' | 'edit'>('create');
  let editorRule = $state<Rule | null>(null);  // rule being edited (edit mode)
  let editorSaving = $state(false);
  let editorError = $state<string | null>(null);

  // Form fields
  let fName = $state('');
  let fPriority = $state(100);
  let fStop = $state(false);
  let fEnabled = $state(true);

  // Matcher fields
  let fFrom = $state('');
  /** When true, the From field builds `from_exact` (literal, no glob
   *  interpretation) instead of `from` (glob). */
  let fFromExact = $state(false);
  let fTo = $state('');
  let fSubject = $state('');
  let fTag = $state('');
  let fScoreAbove = $state('');
  let fScoreBelow = $state('');

  // Action fields
  let fActionKind = $state<ActionKind>('move');
  let fActionArg = $state('');

  // Delete confirm modal
  let deleteTarget = $state<Rule | null>(null);
  let deleteConfirmOpen = $state(false);
  let deleting = $state(false);

  // Enable confirm modal (for risky actions)
  let enableTarget = $state<Rule | null>(null);
  let enableConfirmOpen = $state(false);
  let enabling = $state(false);

  // ── Derived ──────────────────────────────────────────────────────────

  const selectedAccount = $derived(accounts.find((a) => a.id === selectedAccountId) ?? null);

  const folderNames = $derived(
    folders
      .filter((f) => !f.virtual)
      .map((f) => f.folder)
      .sort()
  );

  // Actions that need a destination argument.
  const actionNeedsArg = $derived(
    fActionKind !== 'delete' && fActionKind !== 'unsubscribe'
  );

  // ── Bootstrap ────────────────────────────────────────────────────────

  onMount(async () => {
    try {
      const res = await api.listAccounts();
      accounts = res.accounts;
      if (accounts.length > 0) {
        selectedAccountId = accounts[0].id;
      }
    } catch (e) {
      pageError = e instanceof Error ? e.message : 'Failed to load accounts.';
    } finally {
      loadingAccounts = false;
    }
  });

  $effect(() => {
    if (selectedAccountId) {
      loadRules(selectedAccountId);
      loadFolders(selectedAccountId);
    }
  });

  async function loadRules(accountId: string) {
    loadingRules = true;
    rulesError = null;
    try {
      const res = await rulesApi.list(accountId);
      rules = res.rules;
    } catch (e) {
      rulesError = e instanceof Error ? e.message : 'Failed to load rules.';
    } finally {
      loadingRules = false;
    }
  }

  async function loadFolders(accountId: string) {
    try {
      const res = await api.folders(accountId);
      folders = res.folders ?? [];
    } catch {
      // non-fatal
    }
  }

  // ── Toggle enabled ────────────────────────────────────────────────────

  async function requestToggle(rule: Rule) {
    if (pendingToggle.has(rule.id)) return;

    // Enabling a risky action requires confirm.
    if (!rule.enabled && isHighRiskAction(rule.action)) {
      enableTarget = rule;
      enableConfirmOpen = true;
      return;
    }

    await doToggle(rule);
  }

  async function doToggle(rule: Rule) {
    const accountId = selectedAccountId!;
    pendingToggle = new Set(pendingToggle).add(rule.id);
    try {
      if (rule.enabled) {
        await rulesApi.disable(accountId, rule.id);
      } else {
        await rulesApi.enable(accountId, rule.id);
      }
      await loadRules(accountId);
      showToast(rule.enabled ? 'Rule disabled.' : 'Rule enabled.');
    } catch (e) {
      showToast(e instanceof Error ? e.message : 'Toggle failed.', 'error');
    } finally {
      const next = new Set(pendingToggle);
      next.delete(rule.id);
      pendingToggle = next;
    }
  }

  // ── Enable confirm ────────────────────────────────────────────────────

  async function confirmEnable() {
    if (!enableTarget) return;
    enabling = true;
    await doToggle(enableTarget);
    enabling = false;
    enableTarget = null;
    enableConfirmOpen = false;
  }

  function cancelEnable() {
    enableTarget = null;
    enableConfirmOpen = false;
  }

  // ── Editor open/close ─────────────────────────────────────────────────

  function openCreate() {
    editorMode = 'create';
    editorRule = null;
    editorError = null;
    fName = '';
    fPriority = 100;
    fStop = false;
    fEnabled = true;
    fFrom = '';
    fFromExact = false;
    fTo = '';
    fSubject = '';
    fTag = '';
    fScoreAbove = '';
    fScoreBelow = '';
    fActionKind = 'move';
    fActionArg = folderNames[0] ?? '';
    editorOpen = true;
  }

  function openEdit(rule: Rule) {
    editorMode = 'edit';
    editorRule = rule;
    editorError = null;
    fName = rule.name;
    fPriority = rule.priority;
    fStop = rule.stop;
    fEnabled = rule.enabled;

    // Parse match expr
    const mf = parseMatchExpr(rule.match_expr);
    fFrom = mf.from ?? mf.fromExact ?? '';
    fFromExact = mf.fromExact !== undefined;
    fTo = mf.to ?? '';
    fSubject = mf.subject ?? '';
    fTag = mf.tag ?? '';
    fScoreAbove = mf.scoreAbove ?? '';
    fScoreBelow = mf.scoreBelow ?? '';

    // Parse action
    const af = parseAction(rule.action);
    fActionKind = af.kind;
    fActionArg = af.arg ?? '';

    editorOpen = true;
  }

  function closeEditor() {
    editorOpen = false;
    editorSaving = false;
    editorError = null;
  }

  // ── Save (create / update) ────────────────────────────────────────────

  async function saveRule() {
    const accountId = selectedAccountId!;
    editorSaving = true;
    editorError = null;

    const matchFields: MatchFields = {
      ...(fFromExact ? { fromExact: fFrom } : { from: fFrom }),
      to: fTo,
      subject: fSubject,
      tag: fTag,
      scoreAbove: fScoreAbove,
      scoreBelow: fScoreBelow
    };

    const matchExpr = buildMatchExpr(matchFields);
    if (!matchExpr) {
      editorError = 'At least one match condition is required.';
      editorSaving = false;
      return;
    }

    const action = buildAction({ kind: fActionKind, arg: fActionArg });

    try {
      if (editorMode === 'create') {
        await rulesApi.create(accountId, {
          name: fName.trim(),
          match_expr: matchExpr,
          action,
          priority: fPriority,
          stop: fStop,
          enabled: fEnabled
        });
        showToast('Rule created.');
      } else if (editorRule) {
        await rulesApi.update(accountId, editorRule.id, {
          name: fName.trim(),
          match_expr: matchExpr,
          action,
          priority: fPriority,
          stop: fStop
        });
        showToast('Rule updated.');
      }
      closeEditor();
      await loadRules(accountId);
    } catch (e) {
      editorError =
        e instanceof EnvelopeApiError
          ? `${e.message} (${e.code})`
          : (e instanceof Error ? e.message : 'Save failed.');
    } finally {
      editorSaving = false;
    }
  }

  // ── Delete ────────────────────────────────────────────────────────────

  function openDeleteConfirm(rule: Rule) {
    deleteTarget = rule;
    deleteConfirmOpen = true;
  }

  function cancelDelete() {
    deleteTarget = null;
    deleteConfirmOpen = false;
    deleting = false;
  }

  async function confirmDelete() {
    if (!deleteTarget || !selectedAccountId) return;
    deleting = true;
    try {
      await rulesApi.destroy(selectedAccountId, deleteTarget.id);
      showToast(`Rule "${deleteTarget.name}" deleted.`);
      await loadRules(selectedAccountId);
    } catch (e) {
      showToast(e instanceof Error ? e.message : 'Delete failed.', 'error');
    } finally {
      deleting = false;
      deleteTarget = null;
      deleteConfirmOpen = false;
    }
  }

  // ── Helpers ───────────────────────────────────────────────────────────

  function formatDate(iso: string | null): string {
    if (!iso) return '—';
    try {
      return new Date(iso).toLocaleDateString('en-US', {
        month: 'short',
        day: 'numeric',
        year: 'numeric'
      });
    } catch {
      return iso;
    }
  }
</script>

<div id="rules-page" class="rules-page">
  <!-- ── Header bar ──────────────────────────────────────────────────── -->
  <header class="rules-header">
    <div class="rules-header-left">
      <h1 class="rules-title">Rules</h1>
      {#if !loadingAccounts && accounts.length > 1}
        <select
          id="rules-account-switcher"
          class="account-switcher"
          bind:value={selectedAccountId}
          aria-label="Select account"
        >
          {#each accounts as acct (acct.id)}
            <option value={acct.id}>{acct.display_name ?? acct.name} ({acct.username})</option>
          {/each}
        </select>
      {:else if !loadingAccounts && accounts.length === 1}
        <span class="account-chip">
          <MonoTag>{selectedAccount?.username ?? ''}</MonoTag>
        </span>
      {/if}
    </div>
    <div class="rules-header-right">
      {#if selectedAccountId}
        <Button variant="primary" onclick={openCreate}>New rule</Button>
      {/if}
    </div>
  </header>

  <!-- ── Body ──────────────────────────────────────────────────────── -->
  <div class="rules-body">
    {#if loadingAccounts}
      <div class="rules-loading"><Spinner label="Loading accounts" /> Loading…</div>

    {:else if pageError}
      <div class="rules-page-error" role="alert">
        <p class="error-msg">{pageError}</p>
      </div>

    {:else if accounts.length === 0}
      <EmptyState
        title="No accounts"
        hint="Add an email account first, then come back to create rules."
      />

    {:else if loadingRules}
      <div class="rules-loading"><Spinner label="Loading rules" /> Loading rules…</div>

    {:else if rulesError}
      <div class="rules-page-error" role="alert">
        <p class="error-msg">{rulesError}</p>
        <button class="retry-link" type="button" onclick={() => loadRules(selectedAccountId!)}>Retry</button>
      </div>

    {:else if rules.length === 0}
      <EmptyState
        title="No rules yet"
        hint="Rules run automatically on incoming mail. Create one to get started."
      >
        {#snippet action()}
          <Button variant="primary" onclick={openCreate}>New rule</Button>
        {/snippet}
      </EmptyState>

    {:else}
      <table id="rules-table" class="rules-table" aria-label="Rules">
        <thead>
          <tr>
            <th class="col-on" aria-label="Enabled">On</th>
            <th class="col-name">Name</th>
            <th class="col-match">When</th>
            <th class="col-action">Do</th>
            <th class="col-pri">Priority</th>
            <th class="col-hits">Hits</th>
            <th class="col-ops" aria-label="Actions"></th>
          </tr>
        </thead>
        <tbody>
          {#each rules as rule (rule.id)}
            <tr class="rule-row" class:rule-row--disabled={!rule.enabled}>
              <!-- Enabled toggle -->
              <td class="col-on">
                <button
                  class="toggle-btn"
                  class:toggle-btn--on={rule.enabled}
                  type="button"
                  aria-label={rule.enabled ? 'Disable rule' : 'Enable rule'}
                  aria-pressed={rule.enabled}
                  disabled={pendingToggle.has(rule.id)}
                  onclick={() => requestToggle(rule)}
                  title={rule.enabled ? 'Click to disable' : 'Click to enable'}
                >
                  {#if pendingToggle.has(rule.id)}
                    <span class="toggle-spinner" aria-hidden="true">…</span>
                  {:else}
                    <span class="toggle-pip" aria-hidden="true"></span>
                  {/if}
                </button>
              </td>

              <!-- Name -->
              <td class="col-name">
                <span class="rule-name">{rule.name}</span>
                {#if rule.stop}
                  <span class="stop-badge" title="Stops processing further rules when matched">stop</span>
                {/if}
                {#if !rule.sieve_exportable}
                  <span class="local-badge" title="Uses local-only features (tags/scores/webhooks); can't export to Sieve">local</span>
                {/if}
              </td>

              <!-- Match summary -->
              <td class="col-match">
                <span class="summary-text">{matchSummary(rule.match_expr)}</span>
              </td>

              <!-- Action summary -->
              <td class="col-action">
                <span class="summary-text">{actionSummary(rule.action)}</span>
              </td>

              <!-- Priority -->
              <td class="col-pri">
                <MonoTag>{rule.priority}</MonoTag>
              </td>

              <!-- Hit count + last hit -->
              <td class="col-hits">
                {#if rule.hit_count > 0}
                  <span class="hit-count">{rule.hit_count}</span>
                  {#if rule.last_hit_at}
                    <span class="hit-date">{formatDate(rule.last_hit_at)}</span>
                  {/if}
                {:else}
                  <span class="hit-none">—</span>
                {/if}
              </td>

              <!-- Row operations -->
              <td class="col-ops">
                <div class="row-ops">
                  <button
                    class="op-btn"
                    type="button"
                    aria-label="Edit rule"
                    onclick={() => openEdit(rule)}
                  >Edit</button>
                  <button
                    class="op-btn op-btn--danger"
                    type="button"
                    aria-label="Delete rule"
                    onclick={() => openDeleteConfirm(rule)}
                  >Delete</button>
                </div>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
      <p class="rules-count">{rules.length} rule{rules.length === 1 ? '' : 's'}</p>
    {/if}
  </div>
</div>

<!-- ── Editor modal ───────────────────────────────────────────────────── -->
<Modal
  title={editorMode === 'create' ? 'New rule' : `Edit — ${editorRule?.name ?? ''}`}
  open={editorOpen}
  onclose={closeEditor}
>
  {#snippet children()}
    <form id="rule-editor-form" class="editor-form" onsubmit={(e) => { e.preventDefault(); saveRule(); }}>
      <!-- Name -->
      <div class="field-group">
        <label class="field-label" for="ef-name">Rule name</label>
        <input
          id="ef-name"
          class="field-input"
          type="text"
          placeholder="e.g. Archive GitHub noise"
          required
          maxlength="200"
          bind:value={fName}
        />
      </div>

      <!-- Priority + Stop -->
      <div class="field-row">
        <div class="field-group field-group--sm">
          <label class="field-label" for="ef-priority">Priority</label>
          <input
            id="ef-priority"
            class="field-input"
            type="number"
            min="1"
            max="9999"
            bind:value={fPriority}
          />
          <p class="field-hint">Lower = runs first.</p>
        </div>
        <div class="field-group field-group--check">
          <label class="check-label">
            <input type="checkbox" bind:checked={fStop} />
            Stop after this rule
          </label>
          <p class="field-hint">Skip remaining rules when matched.</p>
        </div>
      </div>

      <!-- ── Matcher builder ──────────────────────────────────────── -->
      <section class="editor-section">
        <h3 class="editor-section-title">Match conditions</h3>
        <p class="editor-section-hint">All filled fields are AND'd together.</p>

        <div class="field-group">
          <label class="field-label" for="ef-from">
            From ({fFromExact ? 'exact address, no wildcards' : 'glob, e.g. *@github.com'})
          </label>
          <input
            id="ef-from"
            class="field-input"
            type="text"
            placeholder={fFromExact ? 'someone@example.com' : '*@example.com'}
            bind:value={fFrom}
          />
          <label class="check-label">
            <input id="ef-from-exact" type="checkbox" bind:checked={fFromExact} />
            Exact match (a literal address, never a glob — `*`/`?` in the address match themselves)
          </label>
        </div>

        <div class="field-group">
          <label class="field-label" for="ef-to">To (glob)</label>
          <input id="ef-to" class="field-input" type="text" placeholder="me@domain.com" bind:value={fTo} />
        </div>

        <div class="field-group">
          <label class="field-label" for="ef-subject">Subject contains (glob)</label>
          <input id="ef-subject" class="field-input" type="text" placeholder="*invoice*" bind:value={fSubject} />
        </div>

        <div class="field-group">
          <label class="field-label" for="ef-tag">Has tag</label>
          <input id="ef-tag" class="field-input" type="text" placeholder="newsletter" bind:value={fTag} />
        </div>

        <div class="field-row">
          <div class="field-group">
            <label class="field-label" for="ef-score-above">Score above (dim=val)</label>
            <input
              id="ef-score-above"
              class="field-input"
              type="text"
              placeholder="spam=0.8"
              bind:value={fScoreAbove}
            />
          </div>
          <div class="field-group">
            <label class="field-label" for="ef-score-below">Score below (dim=val)</label>
            <input
              id="ef-score-below"
              class="field-input"
              type="text"
              placeholder="urgent=0.3"
              bind:value={fScoreBelow}
            />
          </div>
        </div>
      </section>

      <!-- ── Action builder ──────────────────────────────────────── -->
      <section class="editor-section">
        <h3 class="editor-section-title">Action</h3>

        <div class="field-group">
          <label class="field-label" for="ef-action-kind">Action type</label>
          <select id="ef-action-kind" class="field-select" bind:value={fActionKind}>
            <option value="move">Move to folder</option>
            <option value="flag">Flag</option>
            <option value="unflag">Remove flag</option>
            <option value="add_tag">Add tag</option>
            <option value="delete">Delete (permanent)</option>
            <option value="unsubscribe">Unsubscribe + move to Junk</option>
            <option value="snooze">Snooze</option>
            <option value="webhook">Webhook (POST)</option>
          </select>
        </div>

        {#if actionNeedsArg}
          {#if fActionKind === 'move' && folderNames.length > 0}
            <div class="field-group">
              <label class="field-label" for="ef-action-folder">Destination folder</label>
              <select id="ef-action-folder" class="field-select" bind:value={fActionArg}>
                {#each folderNames as folder (folder)}
                  <option value={folder}>{folder}</option>
                {/each}
              </select>
            </div>
          {:else}
            <div class="field-group">
              <label class="field-label" for="ef-action-arg">
                {fActionKind === 'flag' || fActionKind === 'unflag'
                  ? 'Flag name (e.g. \\Flagged, \\Seen)'
                  : fActionKind === 'add_tag'
                    ? 'Tag name'
                    : fActionKind === 'snooze'
                      ? 'When (e.g. tomorrow, monday 9am)'
                      : fActionKind === 'webhook'
                        ? 'Webhook URL'
                        : 'Destination folder'}
              </label>
              <input
                id="ef-action-arg"
                class="field-input"
                type={fActionKind === 'webhook' ? 'url' : 'text'}
                placeholder={
                  fActionKind === 'move'
                    ? 'Archive'
                    : fActionKind === 'flag'
                      ? '\\Flagged'
                      : fActionKind === 'webhook'
                        ? 'https://…'
                        : ''
                }
                bind:value={fActionArg}
              />
              {#if fActionKind === 'webhook'}
                <p class="field-hint">URL is stored server-side and redacted in the UI after saving.</p>
              {/if}
            </div>
          {/if}
        {/if}

        {#if fActionKind === 'delete' || fActionKind === 'unsubscribe'}
          <p class="action-warn">
            ⚠ This action permanently removes messages or unsubscribes from senders.
            Enable the rule only when you're sure it's correct.
          </p>
        {/if}
      </section>

      {#if editorMode === 'create'}
        <div class="field-group">
          <label class="check-label">
            <input type="checkbox" bind:checked={fEnabled} />
            Enable rule immediately
          </label>
        </div>
      {/if}

      {#if editorError}
        <p class="editor-error" role="alert">{editorError}</p>
      {/if}
    </form>
  {/snippet}

  {#snippet footer()}
    <Button variant="ghost" onclick={closeEditor} disabled={editorSaving}>Cancel</Button>
    <Button
      variant="primary"
      type="submit"
      disabled={editorSaving || !fName.trim()}
      onclick={saveRule}
    >
      {#if editorSaving}
        <Spinner label="Saving" />
      {/if}
      {editorMode === 'create' ? 'Create rule' : 'Save changes'}
    </Button>
  {/snippet}
</Modal>

<!-- ── Delete confirm modal ──────────────────────────────────────────── -->
<Modal title="Delete rule" open={deleteConfirmOpen} onclose={cancelDelete}>
  {#snippet children()}
    <p class="confirm-body">
      Delete <strong>{deleteTarget?.name}</strong>?
      This cannot be undone.
    </p>
  {/snippet}
  {#snippet footer()}
    <Button variant="ghost" onclick={cancelDelete} disabled={deleting}>Cancel</Button>
    <Button variant="danger" onclick={confirmDelete} disabled={deleting}>
      {#if deleting}<Spinner label="Deleting" />{/if}
      Delete
    </Button>
  {/snippet}
</Modal>

<!-- ── Enable high-risk action confirm modal ─────────────────────────── -->
<Modal title="Enable rule with risky action" open={enableConfirmOpen} onclose={cancelEnable}>
  {#snippet children()}
    <p class="confirm-body">
      <strong>{enableTarget?.name}</strong> will <strong>{actionSummary(enableTarget?.action ?? '')}</strong>
      on matched messages. Enabling it means future messages may be permanently
      deleted or unsubscribed automatically.
    </p>
    <p class="confirm-body confirm-body--warn">
      Review the match conditions carefully before proceeding.
    </p>
  {/snippet}
  {#snippet footer()}
    <Button variant="ghost" onclick={cancelEnable} disabled={enabling}>Cancel</Button>
    <Button variant="danger" onclick={confirmEnable} disabled={enabling}>
      {#if enabling}<Spinner label="Enabling" />{/if}
      Enable anyway
    </Button>
  {/snippet}
</Modal>

<!-- ── Toast ─────────────────────────────────────────────────────────── -->
{#if toastMsg}
  <div class="toast-region" aria-live="polite">
    <Toast variant={toastVariant} onclose={dismissToast}>
      {toastMsg}
    </Toast>
  </div>
{/if}

<style>
  /* ── Page shell ─────────────────────────────────────────── */
  .rules-page {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 0;
    background: var(--env-paper);
  }

  .rules-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.75rem 1.25rem;
    border-bottom: 1px solid var(--env-rule);
    background: var(--env-paper);
    position: sticky;
    top: 0;
    z-index: 4;
    flex-shrink: 0;
  }

  .rules-header-left {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    min-width: 0;
  }

  .rules-title {
    margin: 0;
    font-size: 0.9375rem;
    font-weight: 600;
    letter-spacing: -0.01em;
    white-space: nowrap;
  }

  .account-switcher {
    font-family: var(--font-sans);
    font-size: 0.8125rem;
    color: var(--env-ink);
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm);
    padding: 0.25rem 0.5rem;
    cursor: pointer;
  }

  .account-chip {
    display: inline-flex;
    align-items: center;
  }

  .rules-header-right {
    flex-shrink: 0;
  }

  .rules-body {
    flex: 1;
    overflow-y: auto;
    padding: 0;
  }

  /* ── Loading / error ─────────────────────────────────────── */
  .rules-loading {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 2rem 1.25rem;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }

  .rules-page-error {
    padding: 1.5rem 1.25rem;
  }

  .error-msg {
    margin: 0 0 0.5rem;
    color: var(--env-warn);
    font-size: 0.875rem;
  }

  .retry-link {
    font-size: 0.8125rem;
    color: var(--env-accent);
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    text-decoration: underline;
  }

  /* ── Rules table ─────────────────────────────────────────── */
  .rules-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.8125rem;
  }

  .rules-table th {
    text-align: left;
    padding: 0.5rem 0.75rem;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
    border-bottom: 1px solid var(--env-rule);
    white-space: nowrap;
    position: sticky;
    top: 0;
    background: var(--env-paper);
    z-index: 2;
  }

  .rules-table td {
    padding: 0.6rem 0.75rem;
    border-bottom: 1px solid var(--env-rule);
    vertical-align: top;
  }

  .rule-row {
    transition: background-color 0.1s ease;
  }

  .rule-row:hover {
    background: var(--env-accent-soft);
  }

  .rule-row--disabled {
    opacity: 0.55;
  }

  /* Column widths */
  .col-on   { width: 48px; text-align: center; }
  .col-name { min-width: 160px; max-width: 240px; }
  .col-match { min-width: 200px; }
  .col-action { min-width: 160px; }
  .col-pri  { width: 72px; text-align: center; }
  .col-hits { width: 100px; }
  .col-ops  { width: 120px; text-align: right; }

  /* ── Enabled toggle ──────────────────────────────────────── */
  .toggle-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 18px;
    border-radius: 9px;
    border: 1.5px solid var(--env-rule);
    background: var(--env-paper);
    cursor: pointer;
    transition: background-color 0.15s ease, border-color 0.15s ease;
    position: relative;
    padding: 0;
  }

  .toggle-btn--on {
    background: var(--env-accent);
    border-color: var(--env-accent);
  }

  .toggle-btn:disabled {
    cursor: not-allowed;
    opacity: 0.6;
  }

  .toggle-pip {
    display: block;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--env-muted);
    transition: transform 0.15s ease, background-color 0.15s ease;
    transform: translateX(-6px);
  }

  .toggle-btn--on .toggle-pip {
    background: #fff;
    transform: translateX(6px);
  }

  .toggle-spinner {
    font-size: 0.75rem;
    color: var(--env-muted);
  }

  /* ── Rule name badges ────────────────────────────────────── */
  .rule-name {
    font-weight: 500;
    display: block;
  }

  .stop-badge,
  .local-badge {
    display: inline-block;
    margin-top: 0.25rem;
    margin-right: 0.25rem;
    font-family: var(--font-mono);
    font-size: 0.6rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    padding: 0.1rem 0.35rem;
    border-radius: var(--radius-xs);
  }

  .stop-badge {
    background: var(--env-pending-soft);
    color: var(--env-pending);
    border: 1px solid var(--env-pending);
  }

  .local-badge {
    background: var(--env-accent-soft);
    color: var(--env-accent);
    border: 1px solid var(--env-accent);
  }

  /* ── Summary text ────────────────────────────────────────── */
  .summary-text {
    display: block;
    color: var(--env-muted);
    line-height: 1.4;
    word-break: break-word;
  }

  /* ── Hit count ───────────────────────────────────────────── */
  .hit-count {
    display: block;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }

  .hit-date {
    display: block;
    font-size: 0.6875rem;
    color: var(--env-muted);
  }

  .hit-none {
    color: var(--env-muted);
  }

  /* ── Row ops ─────────────────────────────────────────────── */
  .row-ops {
    display: flex;
    gap: 0.35rem;
    justify-content: flex-end;
  }

  .op-btn {
    font-family: var(--font-sans);
    font-size: 0.75rem;
    font-weight: 500;
    padding: 0.2rem 0.5rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs);
    background: transparent;
    color: var(--env-ink);
    cursor: pointer;
    white-space: nowrap;
  }

  .op-btn:hover {
    background: var(--env-accent-soft);
    border-color: var(--env-accent);
    color: var(--env-accent);
  }

  .op-btn--danger:hover {
    background: var(--env-warn-soft);
    border-color: var(--env-warn);
    color: var(--env-warn);
  }

  /* ── Rules count footer ──────────────────────────────────── */
  .rules-count {
    margin: 0;
    padding: 0.5rem 0.75rem;
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
    border-top: 1px solid var(--env-rule);
  }

  /* ── Editor modal form ───────────────────────────────────── */
  .editor-form {
    display: flex;
    flex-direction: column;
    gap: 1rem;
  }

  .field-group {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
  }

  .field-group--sm {
    flex: 1;
  }

  .field-group--check {
    flex: 1;
    justify-content: flex-start;
    padding-top: 1.5rem; /* align with input */
  }

  .field-label {
    font-size: 0.75rem;
    font-weight: 600;
    color: var(--env-muted);
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }

  .field-input,
  .field-select {
    font-family: var(--font-sans);
    font-size: 0.8125rem;
    color: var(--env-ink);
    background: var(--env-surface);
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm);
    padding: 0.4rem 0.6rem;
    width: 100%;
    box-sizing: border-box;
  }

  .field-input:focus,
  .field-select:focus {
    outline: 2px solid var(--env-accent);
    outline-offset: 0;
    border-color: var(--env-accent);
  }

  .field-hint {
    margin: 0;
    font-size: 0.6875rem;
    color: var(--env-muted);
  }

  .field-row {
    display: flex;
    gap: 0.75rem;
  }

  .field-row > * {
    flex: 1;
    min-width: 0;
  }

  .check-label {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.8125rem;
    cursor: pointer;
  }

  .editor-section {
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm);
    padding: 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.65rem;
  }

  .editor-section-title {
    margin: 0;
    font-size: 0.75rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
  }

  .editor-section-hint {
    margin: 0;
    font-size: 0.6875rem;
    color: var(--env-muted);
  }

  .action-warn {
    margin: 0;
    padding: 0.5rem 0.65rem;
    font-size: 0.8125rem;
    color: var(--env-warn);
    background: var(--env-warn-soft);
    border-radius: var(--radius-xs);
  }

  .editor-error {
    margin: 0;
    padding: 0.5rem 0.65rem;
    font-size: 0.8125rem;
    color: var(--env-warn);
    background: var(--env-warn-soft);
    border-radius: var(--radius-xs);
  }

  /* ── Confirm modal ───────────────────────────────────────── */
  .confirm-body {
    margin: 0 0 0.5rem;
    font-size: 0.875rem;
    line-height: 1.5;
  }

  .confirm-body--warn {
    color: var(--env-warn);
    font-size: 0.8125rem;
  }

  /* ── Toast region ────────────────────────────────────────── */
  .toast-region {
    position: fixed;
    bottom: 1.5rem;
    right: 1.5rem;
    z-index: 100;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    pointer-events: none;
  }

  .toast-region :global(.env-toast) {
    pointer-events: auto;
  }
</style>
