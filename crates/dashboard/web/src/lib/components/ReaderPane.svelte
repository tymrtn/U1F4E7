<script lang="ts">
  // ReaderPane — full v2 message reader.
  //
  // Owned deliverables in one component:
  //   • Sandboxed HTML rendering via BodyFrame (srcdoc + sandbox=allow-same-origin)
  //   • Text/HTML toggle (session-persistent, default HTML when present)
  //   • Headers block with from/to/cc/date/subject; to+cc collapsed behind Details
  //   • Thread strip (ThreadStrip)
  //   • Attachment list (AttachmentList)
  //   • Read-on-open: a successful open marks the message \Seen through an
  //     intentional STORE mutation (the content fetch stays BODY.PEEK). A
  //     failed load never marks read; re-opening a read message is idempotent.
  //     Evidence/export paths are read-only and never reach this component.
  //   • Explicit read/unread toggle (restores unread after auto-read)
  //   • MonoTag copy affordances for uid + message-id (click-to-copy, toast)
  //   • Drafts intercept: a Drafts-folder deep link never loads the reader. It
  //     resolves the local draft by IMAP UID and hands off to the review
  //     composer, which is the only surface that can edit and send. The message
  //     endpoint is not called (it can 404 while the draft row is fine) and the
  //     draft is never marked Seen.

  import { page } from '$app/state';
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { Spinner, MonoTag, Badge, Toast, Modal, Icon } from '$lib/components';
  import BodyFrame from '$lib/components/BodyFrame.svelte';
  import ThreadStrip from '$lib/components/ThreadStrip.svelte';
  import AttachmentList from '$lib/components/AttachmentList.svelte';
  import Avatar from '$lib/components/Avatar.svelte';
  import {
    fetchMessageDetail,
    fetchThread,
    postFlags,
    isSeen,
    type MessageDetailFull,
    type ThreadMessage
  } from '$lib/reader-api';
  import { api, bulkClient, EnvelopeApiError } from '$lib/api';
  import { looksLikeTrash } from '$lib/folder-kinds';
  import { getMailboxOpsStore } from '$lib/mailbox-ops.svelte';
  import { isDraftsFolder } from '$lib/mailboxes';
  import { folderHints } from '$lib/folder-hints.svelte';
  import { readState } from '$lib/read-state.svelte';
  import { getComposerStore } from '$lib/composer.svelte';

  // ── Route params ──────────────────────────────────────────────────────

  const accountId = $derived(page.params.account ?? '');
  const uid = $derived(Number(page.params.uid ?? 0));
  // Folder resolution, most trustworthy first: an explicit `?folder=` (the only
  // source that can name a mailbox the list never loaded), then the folder the
  // unified list recorded for this exact account+uid, then INBOX. The hint
  // means a link that lost its query string opens the right mailbox instead of
  // 404ing against INBOX — and a Drafts uid still reaches the drafts intercept.
  const folder = $derived(
    page.url.searchParams.get('folder') ||
      folderHints.folderFor(page.params.account ?? null, Number(page.params.uid) || null) ||
      'INBOX'
  );
  const box = $derived(page.params.box ?? 'unified');

  // Friendly name for the source folder, shown as a removable chip beside the
  // subject (A5). 'INBOX' → 'Inbox'; a nested IMAP path shows its leaf.
  const folderLabel = $derived.by(() => {
    const raw = folder || 'INBOX';
    // Split on the server's own hierarchy separator: '/' when present,
    // otherwise '.' (Courier/UW-style 'INBOX.Work'). Splitting on both
    // unconditionally would truncate a dotted leaf like 'Clients/Acme.com'.
    const parts = raw.includes('/') ? raw.split('/') : raw.split('.');
    const leaf = parts.filter(Boolean).pop() ?? raw;
    if (leaf.toUpperCase() === 'INBOX') return 'Inbox';
    return leaf.charAt(0).toUpperCase() + leaf.slice(1);
  });

  // ── Message state ─────────────────────────────────────────────────────

  let message = $state<MessageDetailFull | null>(null);
  let loading = $state(false);
  let error = $state<{ code: string; message: string } | null>(null);
  let loadKey = $state('');

  // ── Drafts state ──────────────────────────────────────────────────────
  // Set only when a Drafts UID has no local draft to hand off to. The reader
  // would render it read-only and mark it Seen, so this card stands in instead.
  let draftFallback = $state<{ uid: number; folder: string } | null>(null);

  // ── Thread state ──────────────────────────────────────────────────────

  let threadMessages = $state<ThreadMessage[]>([]);
  let threadLoading = $state(false);

  // ── Read-toggle state ─────────────────────────────────────────────────

  let flagging = $state(false);
  let localSeen = $state<boolean | null>(null); // null = use message.flags

  let isRead = $derived(() => {
    if (localSeen !== null) return localSeen;
    if (!message) return false;
    return isSeen(message.flags);
  });

  // ── View toggle (text / html) — session-persistent ───────────────────

  const TOGGLE_KEY = 'envelope_reader_body_format';

  function getSessionFormat(): 'html' | 'text' {
    try {
      return (sessionStorage.getItem(TOGGLE_KEY) as 'html' | 'text') || 'html';
    } catch {
      return 'html';
    }
  }

  function setSessionFormat(f: 'html' | 'text') {
    try {
      sessionStorage.setItem(TOGGLE_KEY, f);
    } catch {
      // ignore
    }
    bodyFormat = f;
  }

  let bodyFormat = $state<'html' | 'text'>(getSessionFormat());

  // Derived: what format actually renders (html only if html_body present).
  let effectiveFormat = $derived(() => {
    if (!message) return 'text';
    if (bodyFormat === 'html' && message.html_body) return 'html';
    if (message.text_body) return 'text';
    if (message.html_body) return 'html';
    return 'text';
  });

  // ── Remote images ─────────────────────────────────────────────────────

  let remoteImages = $state(false);
  let remoteBlockedCount = $state(0);

  function onRemoteBlocked(count: number) {
    remoteBlockedCount = count;
  }

  // ── Details disclosure (to/cc) ────────────────────────────────────────

  let headersExpanded = $state(false);

  // ── Toast ─────────────────────────────────────────────────────────────

  let toast = $state<{ text: string; variant: 'ok' | 'warn' } | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | null = null;

  function showToast(text: string, variant: 'ok' | 'warn' = 'ok') {
    if (toastTimer) clearTimeout(toastTimer);
    toast = { text, variant };
    toastTimer = setTimeout(() => {
      toast = null;
    }, 2500);
  }

  // ── Copy affordance ───────────────────────────────────────────────────

  async function copyToClipboard(text: string, label: string) {
    try {
      await navigator.clipboard.writeText(text);
      showToast(`Copied ${label}`);
    } catch {
      showToast('Copy failed', 'warn');
    }
  }

  // ── Load ──────────────────────────────────────────────────────────────

  async function load(acct: string, u: number, f: string) {
    loading = true;
    error = null;
    message = null;
    threadMessages = [];
    localSeen = null;
    localFlagged = null;
    actionError = null;
    remoteImages = false;
    remoteBlockedCount = 0;
    draftFallback = null;

    if (isDraftsFolder(f)) {
      await loadDraft(acct, u, f);
      return;
    }

    try {
      const res = await fetchMessageDetail(acct, u, f);
      message = res.message;

      // Read-on-open: the successful load is the operator's explicit read
      // action. Fire an intentional \Seen STORE (not a BODY[] side effect).
      // Idempotent — skip when the message already carries \Seen.
      if (!isSeen(message.flags)) {
        void markReadOnOpen(acct, u, f);
      } else {
        readState.markRead(acct, f, u);
      }

      // Thread: load if message_id is present (fire-and-forget, no blocking).
      if (message.message_id) {
        threadLoading = true;
        fetchThread(acct, message.message_id)
          .then((thread) => {
            threadMessages = thread?.messages ?? [];
          })
          .catch(() => {
            threadMessages = [];
          })
          .finally(() => {
            threadLoading = false;
          });
      }

      // Auto-select html if present, text otherwise (session default wins).
      if (bodyFormat === 'html' && !message.html_body && message.text_body) {
        // Session prefers html but only text available — render text quietly.
      }
    } catch (e) {
      const err = e as EnvelopeApiError;
      error = {
        code: err.code ?? 'reader_load_error',
        message: err.message ?? 'Failed to load this message.'
      };
    } finally {
      loading = false;
    }
  }

  // Resolve a Drafts-folder UID to its local draft and hand off to the review
  // composer. Deliberately does NOT touch the message endpoint: an IMAP draft
  // can be gone from the mailbox while the local row is still editable, and
  // loading it here would also mark an unsent draft Seen.
  async function loadDraft(acct: string, u: number, f: string) {
    try {
      const res = await api.draftByImapUid(acct, u);
      const localId = res.draft?.id;
      if (localId) {
        await goto(
          `${base}/accounts/${encodeURIComponent(acct)}/drafts/${encodeURIComponent(localId)}`
        );
        return;
      }
      // 200 with no draft would be a backend contract break; say so rather
      // than falling through to a surface that cannot send.
      draftFallback = { uid: u, folder: f };
    } catch (e) {
      const err = e as EnvelopeApiError;
      if (err?.status === 404) {
        draftFallback = { uid: u, folder: f };
      } else {
        error = {
          code: err.code ?? 'draft_lookup_error',
          message: err.message ?? 'Failed to open this draft.'
        };
      }
    } finally {
      loading = false;
    }
  }

  // Mark a freshly-opened unread message \Seen. On success, reflect Read in
  // this pane and in the shared list store so the row un-bolds without a
  // refetch. On failure, leave the message unread and say so (never silent).
  async function markReadOnOpen(acct: string, u: number, f: string) {
    try {
      await postFlags(acct, u, f, ['\\Seen'], []);
    } catch {
      showToast('Couldn’t mark read', 'warn');
      return;
    }
    readState.markRead(acct, f, u);
    // Only reflect in this pane if it's still showing the same message.
    if (accountId === acct && uid === u && folder === f) {
      localSeen = true;
    }
  }

  $effect(() => {
    const key = `${accountId}:${uid}:${folder}`;
    if (accountId && uid && key !== loadKey) {
      loadKey = key;
      load(accountId, uid, folder);
    }
  });

  // ── Mark read/unread ──────────────────────────────────────────────────

  async function toggleRead() {
    if (!message || flagging) return;
    flagging = true;
    const currentlyRead = isRead();
    const add = currentlyRead ? [] : ['\\Seen'];
    const remove = currentlyRead ? ['\\Seen'] : [];
    try {
      await postFlags(accountId, uid, folder, add, remove);
      localSeen = !currentlyRead;
      if (localSeen) readState.markRead(accountId, folder, uid);
      else readState.markUnread(accountId, folder, uid);
      showToast(currentlyRead ? 'Marked unread' : 'Marked read');
    } catch (e) {
      const err = e as EnvelopeApiError;
      showToast(err.message ?? 'Could not update flag', 'warn');
    } finally {
      flagging = false;
    }
  }

  // ── Reply / reply-all / forward ──────────────────────────────────────
  // The composer store is the coordination point: the reader opens it in the
  // right mode with this message as the parent and ComposerDrawer (mounted in
  // the mail layout) does the rest. Reply/reply-all let the server derive
  // recipients and threading headers from the parent; the quoted original is
  // prefilled client-side so the operator sees and can trim what they are
  // answering. Forward is a fresh message, so subject + quoted body are
  // prefilled here.

  const composer = getComposerStore();

  function stripHtml(html: string): string {
    return html
      .replace(/<style[\s\S]*?<\/style>/gi, '')
      .replace(/<script[\s\S]*?<\/script>/gi, '')
      .replace(/<br\s*\/?>/gi, '\n')
      .replace(/<\/(p|div|li|tr|h[1-6])>/gi, '\n')
      .replace(/<[^>]+>/g, '')
      .replace(/&nbsp;/g, ' ')
      .replace(/&amp;/g, '&')
      .replace(/&lt;/g, '<')
      .replace(/&gt;/g, '>')
      .replace(/\n{3,}/g, '\n\n')
      .trim();
  }

  function plainBody(): string {
    if (!message) return '';
    if (message.text_body) return message.text_body;
    if (message.html_body) return stripHtml(message.html_body);
    return '';
  }

  function quoted(text: string): string {
    return text
      .split('\n')
      .map((line) => `> ${line}`)
      .join('\n');
  }

  function replyBodyPrefix(): string {
    if (!message) return '';
    const when = message.date ? fmtAbsolute(message.date) : '';
    const attribution = when
      ? `On ${when}, ${message.from_addr} wrote:`
      : `${message.from_addr} wrote:`;
    return `\n\n${attribution}\n${quoted(plainBody())}`;
  }

  function forwardSubject(): string {
    const subject = message?.subject ?? '';
    return /^\s*fwd?:/i.test(subject) ? subject : `Fwd: ${subject}`;
  }

  function forwardBodyPrefix(): string {
    if (!message) return '';
    const lines = [
      '',
      '',
      '---------- Forwarded message ----------',
      `From: ${message.from_addr}`
    ];
    if (message.date) lines.push(`Date: ${fmtAbsolute(message.date)}`);
    lines.push(`Subject: ${message.subject ?? ''}`);
    const to = addrList(message.to_addrs, message.to_addr);
    if (to) lines.push(`To: ${to}`);
    lines.push('', plainBody());
    return lines.join('\n');
  }

  function openReply(mode: 'reply' | 'reply-all') {
    if (!message) return;
    composer.open(mode, {
      accountId,
      parentUid: uid,
      parentFolder: folder,
      bodyPrefix: replyBodyPrefix()
    });
  }

  function openForward() {
    if (!message) return;
    composer.open('forward', {
      accountId,
      subject: forwardSubject(),
      bodyPrefix: forwardBodyPrefix()
    });
  }

  // ── Quick Reply strip ─────────────────────────────────────────────────
  // A lightweight inline entry that PROMOTES to the review composer — the
  // only surface that sends, so every outbound still flows through attribution
  // and the Governor. It never sends on its own; anything typed here is carried
  // into the composer above the quote.
  let quickReply = $state('');

  const senderShort = $derived.by(() => {
    const f = message?.from_addr ?? '';
    const named = f.match(/^\s*"?([^"<]+?)"?\s*</);
    if (named) return named[1].trim();
    const at = f.indexOf('@');
    return at > 0 ? f.slice(0, at) : f || 'sender';
  });

  function promoteQuickReply() {
    if (!message) return;
    const typed = quickReply.trim();
    // replyBodyPrefix() already opens with a blank line, so concatenate
    // directly — `typed + '\n\n' + quote` would stack three blank lines.
    const quote = replyBodyPrefix();
    composer.open('reply', {
      accountId,
      parentUid: uid,
      parentFolder: folder,
      bodyPrefix: typed ? `${typed}${quote}` : quote
    });
    quickReply = '';
  }

  function quickReplyKeydown(e: KeyboardEvent) {
    if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
      e.preventDefault();
      promoteQuickReply();
    }
  }

  // ── Mailbox actions: archive / delete / star ─────────────────────────
  // Same canonical special-use targets and per-message endpoints BulkToolbar
  // uses (the `/move` boundary resolves `\\Archive` / `\\Trash` to each
  // provider's real folder). Delete is reversible (move to Trash) everywhere
  // except inside Trash, where it is a confirmed permanent delete. A completed
  // move leaves the reader: the list is told to refresh and we return to it.

  const mailboxOps = getMailboxOpsStore();
  let acting = $state(false);
  let actionError = $state<string | null>(null);
  let deleteConfirmOpen = $state(false);
  let localFlagged = $state<boolean | null>(null);

  const inTrash = $derived(looksLikeTrash(folder));

  let isStarred = $derived(() => {
    if (localFlagged !== null) return localFlagged;
    if (!message) return false;
    return message.flags.some((f) => f.toLowerCase() === '\\flagged');
  });

  function leaveToList() {
    void goto(`${base}/mail/${encodeURIComponent(box)}`);
  }

  async function runMailboxOp(
    op: Parameters<typeof bulkClient>[0],
    verb: string
  ): Promise<boolean> {
    if (!message || acting) return false;
    acting = true;
    actionError = null;
    try {
      const result = await bulkClient(op, [{ accountId, uid, folder }]);
      if (result.failed.length > 0) {
        actionError = `Couldn’t ${verb}: ${result.failed[0].error}`;
        return false;
      }
      mailboxOps.operated();
      return true;
    } finally {
      acting = false;
    }
  }

  async function archiveMessage() {
    if (await runMailboxOp({ type: 'move', to_folder: '\\Archive', folder }, 'archive')) {
      leaveToList();
    }
  }

  async function trashMessage() {
    if (await runMailboxOp({ type: 'move', to_folder: '\\Trash', folder }, 'move to Trash')) {
      leaveToList();
    }
  }

  async function deleteForever() {
    deleteConfirmOpen = false;
    if (await runMailboxOp({ type: 'delete', folder }, 'delete')) {
      leaveToList();
    }
  }

  async function toggleStar() {
    if (!message || acting) return;
    acting = true;
    actionError = null;
    const starred = isStarred();
    try {
      await postFlags(
        accountId,
        uid,
        folder,
        starred ? [] : ['\\Flagged'],
        starred ? ['\\Flagged'] : []
      );
      localFlagged = !starred;
      mailboxOps.operated();
    } catch (e) {
      const err = e as EnvelopeApiError;
      actionError = `Couldn’t ${starred ? 'unstar' : 'star'}: ${err.message ?? 'flag update failed'}`;
    } finally {
      acting = false;
    }
  }

  // ── Date formatting ───────────────────────────────────────────────────

  function fmtAbsolute(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleString(undefined, {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit'
    });
  }

  // Exact timestamp for the reader meta line (A5): a document shows precisely
  // when it was sent, down to the second.
  function fmtExact(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleString(undefined, {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
      hour: 'numeric',
      minute: '2-digit',
      second: '2-digit'
    });
  }

  function fmtRelative(iso: string | null): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '';
    const diffMs = Date.now() - d.getTime();
    const diffMins = Math.floor(diffMs / 60000);
    if (diffMins < 1) return 'just now';
    if (diffMins < 60) return `${diffMins}m ago`;
    const diffH = Math.floor(diffMins / 60);
    if (diffH < 24) return `${diffH}h ago`;
    const diffD = Math.floor(diffH / 24);
    if (diffD < 30) return `${diffD}d ago`;
    return '';
  }

  function addrList(multi: string[] | undefined | null, single: string): string {
    if (multi && multi.length > 0) return multi.join(', ');
    return single || '';
  }
</script>

<div class="reader-pane" id="reader-pane">
  <!-- Toast region -->
  {#if toast}
    <div class="reader-toast-region">
      <Toast variant={toast.variant} onclose={() => (toast = null)}>{toast.text}</Toast>
    </div>
  {/if}

  {#if loading}
    <div class="reader-loading">
      <Spinner label="Loading message" />
      <span>Loading…</span>
    </div>
  {:else if error}
    <div class="reader-error" role="alert">
      <p class="reader-error-msg">Couldn't load this message.</p>
      <p class="reader-error-detail">{error.message}</p>
      <p><MonoTag>{error.code}</MonoTag></p>
      <button class="reader-retry" type="button" onclick={() => load(accountId, uid, folder)}>
        Try again
      </button>
    </div>
  {:else if draftFallback}
    <section class="draft-card" id="draft-card">
      <h1 class="draft-card-title">Draft</h1>
      <p class="draft-card-msg">
        This draft only exists in the mailbox on your mail server, so there is no editable copy
        here yet and it can't be sent from this page.
      </p>
      <p class="draft-card-meta">
        <MonoTag>uid {draftFallback.uid}</MonoTag>
        <MonoTag>{draftFallback.folder}</MonoTag>
      </p>
      <a class="draft-card-link" href="{base}/mail/drafts">Open Drafts</a>
    </section>
  {:else if message}
    <article class="msg" id="msg-{message.uid}">
      <!-- ── Header: subject headline · folder chip · right-edge cluster ── -->
      <header class="msg-head">
        <div class="msg-head-line">
          <h1 class="msg-subject">{message.subject || '(no subject)'}</h1>
          <span class="msg-folder-chip">
            <span class="msg-folder-name">{folderLabel}</span>
            <button
              class="msg-folder-x"
              type="button"
              aria-label="Remove from {folderLabel}"
              title="Archive — remove from {folderLabel}"
              disabled={acting}
              onclick={archiveMessage}
            >
              <Icon name="x" size={11} />
            </button>
          </span>
          {#if isRead()}
            <Badge variant="ok">Read</Badge>
          {:else}
            <Badge variant="warn">Unread</Badge>
          {/if}
        </div>

        <div class="msg-cluster" role="group" aria-label="Message actions">
          <button
            class="reader-icon-btn"
            type="button"
            aria-label="Reply"
            title="Reply"
            onclick={() => openReply('reply')}
          >
            <Icon name="reply" size={16} />
          </button>
          <button
            class="reader-icon-btn"
            type="button"
            aria-label="Reply all"
            title="Reply all"
            onclick={() => openReply('reply-all')}
          >
            <Icon name="reply-all" size={16} />
          </button>
          <button
            class="reader-icon-btn"
            type="button"
            aria-label="Forward"
            title="Forward"
            onclick={openForward}
          >
            <Icon name="forward" size={16} />
          </button>
          <span class="cluster-sep" aria-hidden="true"></span>
          <button
            class="reader-icon-btn"
            type="button"
            aria-label="Archive"
            title="Archive"
            disabled={acting}
            onclick={archiveMessage}
          >
            <Icon name="archive" size={16} />
          </button>
          {#if inTrash}
            <button
              class="reader-icon-btn reader-icon-danger"
              type="button"
              aria-label="Delete forever"
              title="Delete forever"
              disabled={acting}
              onclick={() => (deleteConfirmOpen = true)}
            >
              <Icon name="trash" size={16} />
            </button>
          {:else}
            <button
              class="reader-icon-btn"
              type="button"
              aria-label="Delete"
              title="Delete"
              disabled={acting}
              onclick={trashMessage}
            >
              <Icon name="trash" size={16} />
            </button>
          {/if}
          <button
            class="reader-icon-btn reader-star-btn"
            class:is-starred={isStarred()}
            type="button"
            aria-label={isStarred() ? 'Unstar' : 'Star'}
            title={isStarred() ? 'Unstar' : 'Star'}
            aria-pressed={isStarred()}
            disabled={acting}
            onclick={toggleStar}
          >
            {isStarred() ? '★' : '☆'}
          </button>
          <button
            class="reader-icon-btn"
            type="button"
            aria-label={isRead() ? 'Mark unread' : 'Mark read'}
            title={isRead() ? 'Mark unread' : 'Mark read'}
            disabled={flagging}
            onclick={toggleRead}
          >
            <Icon name={isRead() ? 'mail' : 'mail-open'} size={16} />
          </button>
        </div>
      </header>
      {#if actionError}
        <p class="msg-action-error" role="alert">{actionError}</p>
      {/if}
      <Modal
        open={deleteConfirmOpen}
        title="Delete this message forever?"
        onclose={() => (deleteConfirmOpen = false)}
      >
        <p class="msg-delete-warn">
          This permanently deletes the message from Trash. You can’t undo this.
        </p>
        {#snippet footer()}
          <button type="button" class="modal-cancel" onclick={() => (deleteConfirmOpen = false)}>
            Cancel
          </button>
          <button type="button" class="modal-delete" onclick={deleteForever}>
            Permanently delete
          </button>
        {/snippet}
      </Modal>

      <!-- ── Thread strip ───────────────────────────────────────────── -->
      {#if threadLoading || threadMessages.length > 1}
        <ThreadStrip
          messages={threadMessages}
          currentUid={uid}
          {folder}
          {box}
          {accountId}
          loading={threadLoading}
        />
      {/if}

      <!-- ── Meta line: avatar · bold sender · to recipients · exact time ── -->
      <div class="msg-meta-lead">
        <Avatar name={message.from_addr} size={38} />
        <div class="msg-meta-who">
          <div class="msg-meta-fromline">
            <span class="msg-meta-from">{message.from_addr}</span>
            <button
              class="msg-details-toggle"
              type="button"
              aria-expanded={headersExpanded}
              onclick={() => (headersExpanded = !headersExpanded)}
            >
              {headersExpanded ? 'Hide details' : 'Details'}
            </button>
          </div>
          <div class="msg-meta-subline">
            <span class="msg-meta-to">to {addrList(message.to_addrs, message.to_addr)}</span>
            {#if message.date}
              <span class="msg-meta-dot" aria-hidden="true">·</span>
              <time class="msg-meta-date" datetime={message.date} title={fmtRelative(message.date)}>
                {fmtExact(message.date)}
              </time>
            {/if}
          </div>
        </div>
      </div>

      {#if headersExpanded}
        <dl class="msg-meta-details">
          <dt>To</dt>
          <dd>{addrList(message.to_addrs, message.to_addr)}</dd>
          {#if message.cc_addr || (message.cc_addrs && message.cc_addrs.length > 0)}
            <dt>Cc</dt>
            <dd>{addrList(message.cc_addrs, message.cc_addr ?? '')}</dd>
          {/if}
          {#if message.message_id}
            <dt>Message-ID</dt>
            <dd>
              <button
                class="copy-btn"
                type="button"
                onclick={() => copyToClipboard(message!.message_id!, 'Message-ID')}
                title="Copy Message-ID"
              >
                <MonoTag>{message.message_id}</MonoTag>
              </button>
            </dd>
          {/if}
          <dt>UID</dt>
          <dd>
            <button
              class="copy-btn"
              type="button"
              onclick={() => copyToClipboard(String(message!.uid), 'UID')}
              title="Copy UID"
            >
              <MonoTag>uid {message.uid}</MonoTag>
            </button>
          </dd>
        </dl>
      {/if}

      <!-- ── Body toggle + remote image notice ─────────────────────── -->
      <div class="msg-body-toolbar">
        {#if message.html_body && message.text_body}
          <span class="body-toggle" role="group" aria-label="Body format">
            <button
              class="body-toggle-btn"
              class:is-active={bodyFormat === 'html'}
              type="button"
              onclick={() => setSessionFormat('html')}
            >
              HTML
            </button>
            <button
              class="body-toggle-btn"
              class:is-active={bodyFormat === 'text'}
              type="button"
              onclick={() => setSessionFormat('text')}
            >
              Plain text
            </button>
          </span>
        {:else if message.html_body && !message.text_body}
          <span class="body-format-note">HTML only</span>
        {:else if !message.html_body && message.text_body}
          <span class="body-format-note">Plain text only</span>
        {/if}

        {#if effectiveFormat() === 'html' && remoteBlockedCount > 0 && !remoteImages}
          <button
            class="remote-img-btn"
            type="button"
            onclick={() => (remoteImages = true)}
          >
            Load remote images ({remoteBlockedCount} blocked)
          </button>
        {/if}
      </div>

      <!-- ── Body ──────────────────────────────────────────────────── -->
      <div class="msg-body">
        {#if effectiveFormat() === 'html' && message.html_body}
          <BodyFrame
            html={message.html_body}
            {remoteImages}
            {onRemoteBlocked}
          />
        {:else if message.text_body}
          <pre class="msg-text">{message.text_body}</pre>
        {:else}
          <p class="msg-empty">This message has no readable body.</p>
        {/if}
      </div>

      <!-- ── Attachments ────────────────────────────────────────────── -->
      {#if message.attachments && message.attachments.length > 0}
        <AttachmentList
          attachments={message.attachments}
          {accountId}
          uid={message.uid}
          {folder}
        />
      {/if}

      <!-- ── Quick reply — promotes to the review composer ──────────────── -->
      <section class="quick-reply" aria-label="Quick reply">
        <textarea
          class="quick-reply-input"
          bind:value={quickReply}
          placeholder="Reply to {senderShort}…"
          rows="1"
          onkeydown={quickReplyKeydown}
        ></textarea>
        <div class="quick-reply-foot">
          <span class="quick-reply-hint">Opens the composer · every send is attributed</span>
          <button
            class="quick-reply-btn"
            type="button"
            aria-label="Reply in composer"
            onclick={promoteQuickReply}
          >
            <Icon name="reply" size={14} /> Reply
          </button>
        </div>
      </section>
    </article>
  {:else}
    <!-- Empty / no-message-selected state -->
    <div class="reader-empty" id="reader-empty">
      <p class="reader-empty-msg">Select a message to read it.</p>
      <p class="reader-empty-note">Opening a message marks it read.</p>
    </div>
  {/if}
</div>

<style>
  .reader-pane {
    position: relative;
    padding: 1.25rem 1.5rem;
    max-width: 44rem;
    width: 100%;
  }

  /* Toast anchored top-right of the pane */
  .reader-toast-region {
    position: absolute;
    top: 1rem;
    right: 1rem;
    z-index: 10;
  }

  /* Loading */
  .reader-loading {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.8125rem;
    color: var(--env-muted);
    padding: 2rem 0;
  }

  /* Error */
  .reader-error {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .reader-error-msg {
    margin: 0;
    font-weight: 600;
    color: var(--env-warn);
  }
  .reader-error-detail {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .reader-retry {
    align-self: flex-start;
    margin-top: 0.25rem;
    font-size: 0.8125rem;
    color: var(--env-accent);
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    text-decoration: underline;
  }

  /* Draft fallback card — a Drafts uid with no local draft to review. */
  .draft-card {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 0.5rem;
    padding: 1rem 1.15rem;
    border: 1px solid var(--env-accent);
    border-left-width: 3px;
    border-radius: var(--radius-xs, 2px);
    background: var(--env-accent-soft);
  }
  .draft-card-title {
    margin: 0;
    font-size: 1.0625rem;
    font-weight: 600;
    color: var(--env-ink);
  }
  .draft-card-msg {
    margin: 0;
    font-size: 0.875rem;
    line-height: 1.5;
    color: var(--env-ink);
  }
  .draft-card-meta {
    display: flex;
    gap: 0.4rem;
    margin: 0;
    flex-wrap: wrap;
  }
  .draft-card-link {
    font-size: 0.8125rem;
    color: var(--env-accent);
  }

  /* Empty state */
  .reader-empty {
    padding: 2rem 0;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .reader-empty-msg {
    margin: 0;
    font-size: 0.9375rem;
    color: var(--env-muted);
  }
  .reader-empty-note {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
    opacity: 0.7;
  }

  /* Header */
  /* Header: subject headline · folder chip · read badge, then the cluster. */
  .msg-head {
    display: flex;
    flex-direction: column;
    gap: 0.55rem;
    margin-bottom: 0.75rem;
  }
  .msg-head-line {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    flex-wrap: wrap;
  }
  .msg-subject {
    margin: 0;
    font-size: 1.375rem;
    font-weight: 600;
    line-height: 1.25;
    letter-spacing: -0.01em;
    color: var(--env-ink);
    flex: 1 1 auto;
    min-width: 12rem;
    text-wrap: balance;
  }
  .msg-folder-chip {
    display: inline-flex;
    align-items: center;
    gap: 0.2rem;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--env-muted);
    border: 1px solid var(--env-rule);
    border-radius: 999px;
    padding: 0.1rem 0.15rem 0.1rem 0.55rem;
    flex-shrink: 0;
  }
  .msg-folder-x {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border: none;
    background: none;
    color: var(--env-muted);
    cursor: pointer;
    padding: 0.12rem;
    border-radius: 999px;
    line-height: 0;
  }
  .msg-folder-x:hover:not(:disabled) {
    color: var(--env-warn);
    background: var(--env-warn-soft);
  }
  .msg-folder-x:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  /* Right-edge action cluster (A5): icon buttons, verbs on demand. */
  .msg-cluster {
    display: flex;
    align-items: center;
    gap: 0.15rem;
    flex-wrap: wrap;
  }
  .cluster-sep {
    width: 1px;
    height: 18px;
    background: var(--env-rule);
    margin: 0 0.3rem;
  }
  .reader-icon-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 30px;
    border: 1px solid transparent;
    background: none;
    border-radius: var(--radius-sm, 3px);
    color: var(--env-muted);
    cursor: pointer;
    font-size: 0.9rem;
    line-height: 1;
    transition: background 0.1s ease, color 0.1s ease;
  }
  .reader-icon-btn:hover:not(:disabled) {
    background: var(--env-soft);
    color: var(--env-ink);
    border-color: var(--env-rule);
  }
  .reader-icon-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .reader-icon-danger:hover:not(:disabled) {
    color: var(--env-warn);
    border-color: var(--env-warn);
  }
  .reader-star-btn.is-starred {
    color: var(--env-pending);
  }

  .msg-action-error {
    margin: -0.25rem 0 0.75rem;
    font-size: 0.8125rem;
    color: var(--env-warn);
  }
  .msg-delete-warn {
    margin: 0;
    font-size: 0.875rem;
  }
  .modal-cancel,
  .modal-delete {
    font: inherit;
    padding: 0.4rem 0.9rem;
    border-radius: 6px;
    border: 1px solid var(--env-rule);
    background: transparent;
    cursor: pointer;
  }
  .modal-delete {
    color: #fff;
    background: var(--env-warn);
    border-color: var(--env-warn);
  }

  /* Meta lead: avatar · bold sender · to recipients · exact time. */
  .msg-meta-lead {
    display: flex;
    gap: 0.7rem;
    align-items: flex-start;
    margin: 0 0 0.75rem;
    padding-bottom: 0.75rem;
    border-bottom: 1px solid var(--env-rule);
  }
  .msg-meta-who {
    min-width: 0;
    flex: 1;
  }
  .msg-meta-fromline {
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    justify-content: space-between;
  }
  .msg-meta-from {
    font-size: 0.9375rem;
    font-weight: 600;
    color: var(--env-ink);
    overflow-wrap: anywhere;
  }
  .msg-meta-subline {
    display: flex;
    align-items: baseline;
    gap: 0.4rem;
    flex-wrap: wrap;
    margin-top: 0.15rem;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }
  .msg-meta-to {
    overflow-wrap: anywhere;
  }
  .msg-meta-date {
    font-family: var(--font-mono);
    font-size: 0.75rem;
    font-variant-numeric: tabular-nums;
  }
  .msg-meta-dot {
    color: var(--env-rule);
  }

  /* Full-headers table (Details) */
  .msg-meta-details {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.25rem 0.85rem;
    margin: 0 0 0.75rem;
    padding-bottom: 0.75rem;
    border-bottom: 1px solid var(--env-rule);
  }
  .msg-meta-details dt {
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-muted);
    padding-top: 0.1rem;
    white-space: nowrap;
  }
  .msg-meta-details dd {
    margin: 0;
    font-size: 0.8125rem;
    overflow-wrap: anywhere;
  }

  .msg-details-toggle {
    background: none;
    border: none;
    padding: 0;
    font-family: var(--font-mono);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--env-accent);
    cursor: pointer;
    flex-shrink: 0;
  }
  .msg-details-toggle:hover {
    text-decoration: underline;
  }

  .copy-btn {
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    display: inline-flex;
  }
  .copy-btn:hover :global(.env-monotag) {
    border-color: var(--env-accent);
    color: var(--env-accent);
  }

  /* Body toolbar */
  .msg-body-toolbar {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    margin-bottom: 0.5rem;
    flex-wrap: wrap;
  }
  .body-toggle {
    display: inline-flex;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-xs, 2px);
    overflow: hidden;
  }
  .body-toggle-btn {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    background: var(--env-surface);
    color: var(--env-muted);
    border: none;
    padding: 0.2rem 0.5rem;
    cursor: pointer;
    transition: background 0.1s ease, color 0.1s ease;
  }
  .body-toggle-btn.is-active {
    background: var(--env-accent);
    color: #fff;
  }
  .body-toggle-btn:hover:not(.is-active) {
    background: var(--env-accent-soft);
    color: var(--env-accent);
  }
  .body-format-note {
    font-size: 0.6875rem;
    color: var(--env-muted);
    font-family: var(--font-mono);
  }
  .remote-img-btn {
    font-size: 0.75rem;
    color: var(--env-accent);
    background: none;
    border: 1px solid var(--env-accent);
    border-radius: var(--radius-xs, 2px);
    padding: 0.15rem 0.5rem;
    cursor: pointer;
  }
  .remote-img-btn:hover {
    background: var(--env-accent-soft);
  }

  /* Body */
  .msg-body {
    margin-bottom: 1rem;
  }
  .msg-text {
    margin: 0;
    /* ~70ch measure (A5): a document reads best at a bounded line length. */
    max-width: 70ch;
    font-family: var(--font-sans);
    font-size: 0.9375rem;
    line-height: 1.6;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    color: var(--env-ink);
  }
  .msg-empty {
    margin: 0;
    font-size: 0.8125rem;
    color: var(--env-muted);
  }

  /* Quick reply — promotes to the review composer (the only send path). */
  .quick-reply {
    margin-top: 1.25rem;
    padding-top: 1rem;
    border-top: 1px solid var(--env-rule);
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    max-width: 70ch;
  }
  .quick-reply-input {
    width: 100%;
    min-height: 2.5rem;
    resize: vertical;
    padding: 0.6rem 0.75rem;
    border: 1px solid var(--env-rule);
    border-radius: var(--radius-md, 5px);
    background: var(--env-surface);
    color: var(--env-ink);
    font-family: var(--font-sans);
    font-size: 0.875rem;
    line-height: 1.5;
  }
  .quick-reply-input:focus-visible {
    outline: 2px solid color-mix(in srgb, var(--env-accent) 55%, white);
    outline-offset: 1px;
    border-color: var(--env-accent);
  }
  .quick-reply-foot {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
    flex-wrap: wrap;
  }
  .quick-reply-hint {
    font-family: var(--font-mono);
    font-size: 0.6875rem;
    color: var(--env-muted);
  }
  .quick-reply-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    font-size: 0.8125rem;
    font-weight: 600;
    color: #fff;
    background: var(--env-accent);
    border: 1px solid var(--env-accent);
    border-radius: var(--radius-sm, 3px);
    padding: 0.35rem 0.85rem;
    cursor: pointer;
  }
  .quick-reply-btn:hover {
    background: color-mix(in srgb, var(--env-accent) 88%, black);
  }
</style>
