<script lang="ts">
  // Contextual account drawer: details + actions for a single account.
  //
  // - "Reconnect" (POST /accounts/{id}/verify) renders ONLY when the account is
  //   unhealthy — a healthy account never shows a reconnect affordance.
  // - "Delete account" (DELETE /accounts/{id}) lives behind a typed-name
  //   confirm Modal so it can never sit adjacent to a healthy row.
  import Drawer from './Drawer.svelte';
  import Modal from './Modal.svelte';
  import Button from './Button.svelte';
  import Badge from './Badge.svelte';
  import MonoTag from './MonoTag.svelte';
  import Spinner from './Spinner.svelte';
  import { api, EnvelopeApiError, type Account, type AccountHealth } from '$lib/api';

  let {
    account,
    health,
    open,
    onclose,
    onchanged
  }: {
    account: Account | null;
    health: AccountHealth;
    open: boolean;
    onclose: () => void;
    /** Fired after a successful verify or delete so the rail can refresh. */
    onchanged: () => void;
  } = $props();

  let reconnecting = $state(false);
  let reconnectResult = $state<{ ok: boolean; message: string } | null>(null);
  let reconnectError = $state<{ code: string; message: string } | null>(null);

  let confirmOpen = $state(false);
  let typedName = $state('');
  let deleting = $state(false);
  let deleteError = $state<{ code: string; message: string } | null>(null);

  // Reset transient action state whenever the drawer target changes.
  $effect(() => {
    account;
    reconnectResult = null;
    reconnectError = null;
    confirmOpen = false;
    typedName = '';
    deleteError = null;
  });

  const confirmMatches = $derived(!!account && typedName.trim() === account.username);

  async function reconnect() {
    if (!account) return;
    reconnecting = true;
    reconnectResult = null;
    reconnectError = null;
    try {
      const res = await api.verifyAccount(account.id);
      reconnectResult = res.ok
        ? { ok: true, message: 'IMAP connection verified.' }
        : { ok: false, message: res.error ?? 'Reconnect failed.' };
      if (res.ok) onchanged();
    } catch (e) {
      const err = e as EnvelopeApiError;
      reconnectError = { code: err.code ?? 'unknown', message: err.message };
    } finally {
      reconnecting = false;
    }
  }

  async function confirmDelete() {
    if (!account || !confirmMatches) return;
    deleting = true;
    deleteError = null;
    try {
      await api.deleteAccount(account.id);
      confirmOpen = false;
      onchanged();
      onclose();
    } catch (e) {
      const err = e as EnvelopeApiError;
      deleteError = { code: err.code ?? 'unknown', message: err.message };
    } finally {
      deleting = false;
    }
  }
</script>

<Drawer {open} title={account?.display_name || account?.name || 'Account'} {onclose}>
  {#if account}
    <div class="acct-detail">
      <div class="acct-head">
        <span class="acct-email"><MonoTag>{account.username}</MonoTag></span>
        {#if health === 'unhealthy'}
          <Badge variant="warn">Needs reconnect</Badge>
        {:else if health === 'healthy'}
          <Badge variant="ok">Connected</Badge>
        {/if}
      </div>

      <dl class="acct-fields">
        <dt>IMAP</dt>
        <dd><MonoTag>{account.imap_host}:{account.imap_port}</MonoTag></dd>
        <dt>SMTP</dt>
        <dd><MonoTag>{account.smtp_host}:{account.smtp_port}</MonoTag></dd>
      </dl>

      {#if health === 'unhealthy'}
        <section class="acct-action">
          <p class="acct-action-note">
            The last recorded connection for this mailbox failed. Reconnect
            re-checks IMAP auth with the stored credential.
          </p>
          <div class="acct-action-row">
            <Button variant="primary" onclick={reconnect} disabled={reconnecting}>
              {#if reconnecting}<Spinner label="Reconnecting" />{/if}
              Reconnect
            </Button>
          </div>
          {#if reconnectResult}
            <p class="acct-result" class:is-ok={reconnectResult.ok} class:is-err={!reconnectResult.ok}>
              {reconnectResult.message}
            </p>
          {/if}
          {#if reconnectError}
            <p class="acct-result is-err">
              Reconnect error: {reconnectError.message}
              <MonoTag>{reconnectError.code}</MonoTag>
            </p>
          {/if}
        </section>
      {/if}

      <section class="acct-danger">
        <Button variant="danger" onclick={() => (confirmOpen = true)}>Delete account</Button>
        <p class="acct-danger-note">
          Removes the account and its stored credential from Envelope. Mailbox
          contents on the server are untouched.
        </p>
      </section>
    </div>
  {/if}
</Drawer>

<Modal open={confirmOpen} title="Delete this account?" onclose={() => (confirmOpen = false)}>
  {#if account}
    <p class="confirm-lede">
      Type the account address to confirm deletion. This removes the stored
      credential and cannot be undone.
    </p>
    <p class="confirm-target"><MonoTag>{account.username}</MonoTag></p>
    <input
      class="confirm-input"
      type="text"
      autocomplete="off"
      spellcheck="false"
      placeholder="Type the account address"
      aria-label="Type the account address to confirm"
      bind:value={typedName}
    />
    {#if deleteError}
      <p class="acct-result is-err">
        Delete failed: {deleteError.message} <MonoTag>{deleteError.code}</MonoTag>
      </p>
    {/if}
  {/if}
  {#snippet footer()}
    <Button variant="ghost" onclick={() => (confirmOpen = false)}>Cancel</Button>
    <Button variant="danger" onclick={confirmDelete} disabled={!confirmMatches || deleting}>
      {#if deleting}<Spinner label="Deleting" />{/if}
      Delete account
    </Button>
  {/snippet}
</Modal>

<style>
  .acct-detail {
    display: flex;
    flex-direction: column;
    gap: 1rem;
  }
  .acct-head {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
  }
  .acct-fields {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.3rem 0.75rem;
    margin: 0;
    align-items: center;
  }
  .acct-fields dt {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
  }
  .acct-fields dd {
    margin: 0;
  }
  .acct-action,
  .acct-danger {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    padding-top: 0.85rem;
    border-top: 1px solid var(--env-rule);
  }
  .acct-action-note,
  .acct-danger-note {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
    line-height: 1.4;
  }
  .acct-action-row {
    display: flex;
  }
  .acct-result {
    margin: 0;
    font-size: 0.8125rem;
    display: flex;
    align-items: center;
    gap: 0.35rem;
    flex-wrap: wrap;
  }
  .acct-result.is-ok {
    color: var(--env-accent);
  }
  .acct-result.is-err {
    color: var(--env-warn);
  }
  .confirm-lede {
    margin: 0 0 0.5rem;
  }
  .confirm-target {
    margin: 0 0 0.65rem;
  }
  .confirm-input {
    width: 100%;
    box-sizing: border-box;
    font-family: var(--font-mono);
    font-size: 0.8125rem;
    padding: 0.4rem 0.55rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-sm, 3px);
    background: var(--env-paper);
    color: var(--env-ink);
  }
  .confirm-input:focus {
    outline: none;
    border-color: var(--env-accent);
  }
</style>
