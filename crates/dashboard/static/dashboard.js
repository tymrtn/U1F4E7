// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Safe rendering notes:
// - HTML email bodies render inside a sandboxed iframe (no scripts, no forms,
//   no top-navigation). Never assigned to innerHTML on the dashboard DOM.
//   The sandbox grants `allow-same-origin` ONLY so we can read the rendered
//   document height and size the frame to its content (Gmail-style). This is
//   safe because `allow-scripts` is never set and sanitizeEmailHtml() strips
//   <script>/<style>/<form>/etc — no email-controlled code can ever execute,
//   so same-origin confers no escape path.
// - Every piece of user-supplied text (subjects, addresses, filenames,
//   body excerpts) goes through textContent, never innerHTML.
// - DOM trees are built with createElement/appendChild, not template strings.

// ── State ──────────────────────────────────────────────────────────
const state = {
  accounts: [],
  currentView: 'unified',
  currentSmartMailbox: 'unified',
  currentAccount: null,
  folders: [],
  accountFolders: {},
  folderLoadState: {},
  currentFolder: 'INBOX',
  messages: [],
  unifiedMeta: null,
  currentMessage: null,
  drafts: [],
  currentDraft: null,
  snoozed: [],
  composeMode: 'new',
  composeParent: null,
  composeDraft: null,
  composerSending: false,
  pendingAttachments: [],
  bodyFormat: 'text',
  showAllAccounts: false,
  searchQuery: '',
  rules: [],
  route: null,
  cockpitStatus: 'select an account',
  cockpitMessage: null,
  cockpit: null,
  stats: {},
  cockpitExpanded: false,
  loadRemoteImages: false,
  selectedMessages: new Set(),
  starredMessages: new Set(),
  renderedMessageKeys: [],
  focusedMessageKey: null,
  lastSelectedKey: null,
  bulkStatusTimer: null,
};

// ── Auto-refresh ───────────────────────────────────────────────────
let autoRefreshTimer = null;

function scheduleAutoRefresh(delayMs) {
  clearTimeout(autoRefreshTimer);
  autoRefreshTimer = null;
  if (document.visibilityState !== 'visible') return;
  autoRefreshTimer = setTimeout(async () => {
    autoRefreshTimer = null;
    if (document.visibilityState !== 'visible') {
      scheduleAutoRefresh(delayMs);
      return;
    }
    if (state.searchQuery || anyModalOpen()) {
      scheduleAutoRefresh(delayMs);
      return;
    }
    if (state.selectedMessages.size > 0) {
      // Don't interrupt mid-selection or in-flight bulk operations.
      scheduleAutoRefresh(delayMs);
      return;
    }
    try {
      if (isUnifiedView()) {
        await loadUnifiedInbox({ refresh: false });
      } else if (state.currentAccount) {
        await loadMessages();
      }
    } catch (_) {
      // Background refresh errors are silent; next cycle will retry.
    }
    scheduleAutoRefresh(delayMs);
  }, delayMs);
}

function cancelAutoRefresh() {
  clearTimeout(autoRefreshTimer);
  autoRefreshTimer = null;
}

const SMART_MAILBOXES = [
  { key: 'unified', label: 'Unified Inbox' },
  { key: 'attention', label: 'Needs Attention' },
  { key: 'snoozed', label: 'Snoozed' },
  { key: 'sent', label: 'Sent' },
  { key: 'drafts', label: 'Drafts' },
  { key: 'all_mail', label: 'All Mail' },
];

const SMART_FOLDER_DEFAULTS = {
  sent: 'Sent',
  drafts: 'Drafts',
  all_mail: 'All Mail',
};

const SMART_FOLDER_ALIASES = {
  sent: ['sent', 'sent mail', 'sent items', 'sent messages'],
  drafts: ['drafts', 'draft'],
  all_mail: ['all mail', 'all', 'archive', 'archives'],
};

// ── Dashboard routes ───────────────────────────────────────────────
function safeDecodeSegment(segment) {
  try { return decodeURIComponent(segment); }
  catch (_) { return segment; }
}

function normalizeSlug(value) {
  return String(value || '').trim().toLowerCase();
}

function resolveAccountSlug(slug) {
  const wanted = normalizeSlug(slug);
  if (!wanted) return null;
  return state.accounts.find(acct => {
    return [acct.id, acct.username, acct.name, acct.display_name]
      .filter(Boolean)
      .some(value => normalizeSlug(value) === wanted);
  }) || null;
}

function dashboardPathForDraft(draft) {
  const accountId = draft.account_id || (state.currentAccount && state.currentAccount.id);
  if (draft && draft.imap_uid && draft.folder) {
    return `/accounts/${encodeURIComponent(accountId)}/messages/${encodeURIComponent(draft.imap_uid)}?folder=${encodeURIComponent(draft.folder)}`;
  }
  if (!accountId || !draft.id) return '#';
  return `/accounts/${encodeURIComponent(accountId)}/drafts/${encodeURIComponent(draft.id)}`;
}

function focusAgentCockpit() {
  const panel = $('agent-cockpit');
  if (!panel) return;
  panel.scrollIntoView({ behavior: 'smooth', block: 'start' });
  panel.focus({ preventScroll: true });
}

// ── CSRF token (double-submit) ─────────────────────────────────────
// Mutating /api requests that are NOT authenticated by a bearer token (open
// loopback mode, or a tailnet identity behind `tailscale serve`) must echo a
// CSRF token in the X-Envelope-CSRF header matching the cookie the server set.
let csrfToken = null;
const MUTATING_METHODS = new Set(['POST', 'PUT', 'DELETE', 'PATCH']);

async function primeCsrf() {
  try {
    const res = await fetch('/api/csrf', { headers: { 'Accept': 'application/json' } });
    if (res.ok) {
      const data = await res.json();
      csrfToken = data && data.token ? data.token : null;
    }
  } catch (e) {
    // Non-fatal: mutating calls will retry priming on demand.
    csrfToken = null;
  }
  return csrfToken;
}

// ── Fetch helper ───────────────────────────────────────────────────
async function api(method, path, body) {
  const opts = { method, headers: { 'Accept': 'application/json' } };
  if (body !== undefined) {
    opts.headers['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  if (MUTATING_METHODS.has(method)) {
    if (!csrfToken) await primeCsrf();
    if (csrfToken) opts.headers['X-Envelope-CSRF'] = csrfToken;
  }
  const res = await fetch('/api' + path, opts);
  const text = await res.text();
  if (!res.ok) {
    throw new Error(text || `${method} ${path} failed: ${res.status}`);
  }
  try { return text ? JSON.parse(text) : null; }
  catch (e) { return text; }
}

// ── DOM helpers ────────────────────────────────────────────────────
function el(tag, opts = {}, children = []) {
  const node = document.createElement(tag);
  if (opts.class) node.className = opts.class;
  if (opts.text) node.textContent = opts.text;
  if (opts.title) node.title = opts.title;
  if (opts.href) node.href = opts.href;
  if (opts.href) node.rel = 'noreferrer';
  if (opts.type) node.type = opts.type;
  if (opts.disabled) node.disabled = true;
  if (opts.aria) for (const [k, v] of Object.entries(opts.aria)) node.setAttribute(`aria-${k}`, v);
  if (opts.role) node.setAttribute('role', opts.role);
  if (opts.onclick) node.onclick = opts.onclick;
  if (opts.style) Object.assign(node.style, opts.style);
  if (opts.data) for (const [k, v] of Object.entries(opts.data)) node.dataset[k] = v;
  for (const child of children) if (child) node.appendChild(child);
  return node;
}

function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

function $(id) { return document.getElementById(id); }

// ── Toasts ─────────────────────────────────────────────────────────
function toast(message, kind = '') {
  const region = $('toast-region');
  const t = el('div', { class: `toast ${kind}`, text: message });
  region.appendChild(t);
  setTimeout(() => t.remove(), 4000);
}

function setRefresh(text) {
  const stamp = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  $('last-refresh').textContent = text === 'ok' ? `ok · ${stamp}` : text;
}

function setStat(id, value) {
  $(id).textContent = value === undefined || value === null ? 'unavailable' : String(value);
}

function normalizeFolderName(name) {
  return String(name || '').toLowerCase().replace(/^\[gmail\]\//, '').replace(/^\[mailbox\]\//, '');
}

function displayFolderName(name) {
  const raw = String(name || '');
  const normalized = normalizeFolderName(raw);
  const aliases = {
    'inbox': 'Inbox',
    'all mail': 'All Mail',
    'sent mail': 'Sent',
    'sent': 'Sent',
    'drafts': 'Drafts',
    'spam': 'Spam',
    'junk': 'Junk',
    'trash': 'Trash',
    'bin': 'Trash',
  };
  return aliases[normalized] || raw.replace(/^\[Gmail\]\//, '').replace(/^\[Mailbox\]\//, '');
}

function folderMeta(folderName) {
  return state.folders.find(f => f.folder === folderName) || null;
}

function findFolderByKind(kind) {
  return state.folders.find(f => normalizeFolderName(f.folder) === kind) || null;
}

function folderMatchesSmart(folder, key) {
  const normalized = normalizeFolderName(folder?.folder || folder);
  return (SMART_FOLDER_ALIASES[key] || []).includes(normalized);
}

function findFolderForSmart(key) {
  return state.folders.find(f => folderMatchesSmart(f, key)) || null;
}

function folderNameForSmart(key) {
  return findFolderForSmart(key)?.folder || SMART_FOLDER_DEFAULTS[key] || 'INBOX';
}

function folderCountText(f) {
  if (!f) return 'unavailable';
  const unseen = f.unseen || 0;
  const exists = f.exists || 0;
  return unseen > 0 ? `${unseen} unread / ${exists} total` : `${exists} total`;
}

function updateCurrentFolderStats() {
  const inbox = findFolderByKind('inbox');
  setStat('stat-unread', inbox ? (inbox.unseen || 0) : null);
  // stat-drafts counts local DB active drafts (status: draft/pending_review),
  // loaded via /stats — not the IMAP Drafts folder message count.
}

function selectedAccountForSmartMailbox() {
  return state.currentAccount || (state.accounts.length === 1 ? state.accounts[0] : null);
}

function setSearchState(query) {
  state.searchQuery = query || '';
  const active = $('search-active');
  const clear = $('btn-search-clear');
  if (state.searchQuery) {
    active.textContent = 'filtered';
    active.title = state.searchQuery;
    active.classList.remove('hidden');
    clear.classList.remove('hidden');
    // Suspend auto-refresh while a search is active to avoid clobbering results.
    cancelAutoRefresh();
  } else {
    active.textContent = '';
    active.title = '';
    active.classList.add('hidden');
    clear.classList.add('hidden');
  }
}

function ruleActionText(action) {
  if (!action) return 'no action';
  try {
    const parsed = typeof action === 'string' ? JSON.parse(action) : action;
    if (typeof parsed === 'string') return parsed;
    const entries = Object.entries(parsed);
    if (entries.length === 0) return JSON.stringify(parsed);
    const [kind, value] = entries[0];
    if (value === null || value === undefined) return kind;
    return `${kind} → ${value}`;
  } catch (_) {
    return String(action);
  }
}

function ruleMatchText(matchExpr) {
  if (!matchExpr) return 'match expression unavailable';
  try {
    const parsed = typeof matchExpr === 'string' ? JSON.parse(matchExpr) : matchExpr;
    return JSON.stringify(parsed);
  } catch (_) {
    return String(matchExpr);
  }
}

function ruleActionRisk(action) {
  const text = ruleActionText(action).toLowerCase();
  if (text.includes('delete') || text.includes('trash') || text.includes('junk')) return 'high-risk';
  if (text.includes('webhook') || text.includes('unsubscribe') || text.includes('reject') || text.includes('sieve')) return 'gated-risk';
  return 'lower-risk';
}

function ruleAccountText(rule) {
  return accountDisplay(rule.account_id, state.currentAccount?.username || rule.account_id);
}

function currentRulesCli() {
  if (!state.currentAccount) return '';
  return `envelope rule list --account ${state.currentAccount.username || state.currentAccount.id} --json`;
}

function copyText(text) {
  if (!text) return;
  navigator.clipboard?.writeText(text).then(
    () => toast('Copied CLI command', 'success'),
    () => toast(text, 'error')
  );
}

function isUnifiedView() {
  return state.currentView === 'unified';
}

function accountById(accountId) {
  return state.accounts.find(a => a.id === accountId) || null;
}

function accountDisplay(accountId, fallback = '') {
  const account = accountById(accountId);
  return account?.display_name || account?.username || fallback || accountId || 'account';
}

function messageAccountId(message = state.currentMessage) {
  return message?.account_id || state.currentAccount?.id || null;
}

function messageFolder(message = state.currentMessage) {
  return message?.folder || state.currentFolder || 'INBOX';
}

function messageAccountLabel(message) {
  return message?.account_display_name || message?.account_username || accountDisplay(message?.account_id, message?.account_id);
}

function isMessageUnread(message) {
  if (typeof message?.unread === 'boolean') return message.unread;
  return !(message?.flags || []).some(f => String(f).toLowerCase().includes('seen'));
}

function setMessageUnread(message, unread) {
  if (!message) return;
  message.unread = unread;
  const flags = (message.flags || []).filter(f => !String(f).toLowerCase().includes('seen'));
  if (!unread) flags.push('\\Seen');
  message.flags = flags;
}

function threadContext(message) {
  return message?.thread_context || message?.thread || null;
}

function threadMetaText(message) {
  const t = threadContext(message);
  if (!t) return '';
  const parts = [];
  if (t.thread_count) parts.push(`${t.thread_count} msg${t.thread_count === 1 ? '' : 's'}`);
  if (t.last_activity) parts.push(`latest ${formatDate(t.last_activity)}`);
  if (t.has_reply) parts.push('replied');
  return parts.join(' · ');
}

function threadContextUrl(message) {
  const accountId = messageAccountId(message);
  const mid = message?.message_id;
  return accountId && mid ? `/api/accounts/${encodeURIComponent(accountId)}/threads/${encodeURIComponent(mid)}` : '';
}

function messageKey(message) {
  const accountId = messageAccountId(message) || 'account';
  const folder = messageFolder(message) || 'INBOX';
  const uid = message?.uid ?? message?.id ?? message?.message_id ?? '';
  return [accountId, folder, uid].map(part => encodeURIComponent(String(part))).join('::');
}

function safeCliValue(value) {
  const text = String(value || '');
  return /^[A-Za-z0-9_./:@+-]+$/.test(text) ? text : JSON.stringify(text);
}

function accountCliValue(acct) {
  return safeCliValue(acct?.username || acct?.id || 'account');
}

function accountDomain(acct) {
  return acct?.domain || String(acct?.username || '').split('@')[1] || '';
}

function accountHost(value) {
  return String(value || '').trim().replace(/^https?:\/\//, '').replace(/\/.*$/, '');
}

function accountProviderCapabilities(acct) {
  const provider = acct?.provider_name || acct?.provider || acct?.provider_type || accountDomain(acct);
  const parts = [];
  if (provider) parts.push(provider);
  const imap = accountHost(acct?.imap_host);
  const smtp = accountHost(acct?.smtp_host);
  if (imap) parts.push(`imap ${imap}`);
  if (smtp) parts.push(`smtp ${smtp}`);
  return parts.length ? parts.slice(0, 3).join(' · ') : 'capabilities unknown';
}

function accountSignalMatches(signal, acct) {
  const ids = new Set([acct?.id, acct?.username].filter(Boolean).map(String));
  return ids.has(String(signal?.account_id || ''))
    || ids.has(String(signal?.account_username || ''))
    || ids.has(String(signal?.username || ''));
}

function unifiedAccountSignal(acct) {
  return (state.unifiedMeta?.accounts || []).find(item => accountSignalMatches(item, acct)) || null;
}

function unifiedAccountError(acct) {
  return (state.unifiedMeta?.errors || []).find(item => accountSignalMatches(item, acct)) || null;
}

function cockpitAccountItems(collection, acct) {
  return (collection || []).filter(item => accountSignalMatches(item, acct));
}

function classifyAccountHealthFailure(reason, fallback = 'unavailable') {
  const text = String(reason || '').toLowerCase();
  // "cache missing / refresh required" is an informational state — the
  // local cache hasn't been populated yet. It's not an auth or connectivity
  // failure, so classify it as its own state (not in HEALTH_NEEDS_ACTION).
  if (text.includes('cache missing') || text.includes('refresh required')) return 'cache_missing';
  if (/(rate|throttl|too many|429|quota|limit exceeded)/.test(text)) return 'rate_limited';
  if (/(auth|credential|password|login|oauth|unauthorized|forbidden|permission|sasl)/.test(text)) return 'auth_failed';
  if (/(reconnect|reconnecting)/.test(text)) return 'reconnecting';
  return fallback;
}

function sanitizedAccountHealthReason(reason, status) {
  const text = String(reason || '').toLowerCase();
  const defaults = {
    healthy: 'local signals healthy',
    syncing: 'loading local metadata',
    stale: 'cached index is stale',
    cache_missing: 'click Refresh to load mailbox',
    auth_failed: 'authentication failed',
    rate_limited: 'provider rate limited',
    unavailable: 'mailbox unavailable',
    reconnecting: 'reconnecting',
  };
  if (!text) return defaults[status] || 'status unknown';
  if (text.includes('cache missing') || text.includes('refresh required')) return defaults.cache_missing;
  if (/(rate|throttl|too many|429|quota|limit exceeded)/.test(text)) return defaults.rate_limited;
  if (/(auth|credential|password|login|oauth|unauthorized|forbidden|permission|sasl)/.test(text)) return defaults.auth_failed;
  if (text.includes('cache freshness unknown')) return 'cache freshness unknown';
  if (/(timeout|network|dns|tls|certificate|connection|imap)/.test(text)) return defaults.unavailable;
  return defaults[status] || 'status unknown';
}

function accountFolderSummary(acct) {
  const data = currentAccountFolderData(acct?.id);
  const folders = sortedFolders(data?.folders || []);
  if (!folders.length && !data?.snoozed_virtual) return '';
  const inbox = folders.find(f => normalizeFolderName(f.folder) === 'inbox');
  const parts = [];
  if (folders.length) parts.push(`${folders.length} folder${folders.length === 1 ? '' : 's'}`);
  if (inbox) {
    const unseen = inbox.unseen || 0;
    parts.push(unseen > 0 ? `inbox ${unseen} unread / ${inbox.exists || 0}` : `inbox ${inbox.exists || 0}`);
  }
  if (data?.snoozed_virtual?.exists) parts.push(`${data.snoozed_virtual.exists} snoozed`);
  return parts.slice(0, 2).join(' · ');
}

function accountSyncMeta(acct, health) {
  // For cache_missing accounts, suppress the "unavailable · 0 unread · ..."
  // noise. A single helpful hint is enough — the badge already signals
  // state, and the Refresh button is the action.
  if (health.state === 'cache_missing') {
    return 'click Refresh to load mailbox';
  }
  const unified = unifiedAccountSignal(acct);
  const folderState = state.folderLoadState[acct?.id];
  const parts = [];
  if (health.state === 'syncing' || folderState === 'loading') parts.push('syncing local metadata');
  if (unified?.freshness) parts.push(String(unified.freshness).replaceAll('_', ' '));
  if (unified?.indexed_at) parts.push(`indexed ${formatDate(unified.indexed_at)}`);
  if (Number.isFinite(Number(unified?.unread_count))) parts.push(`${unified.unread_count} unread`);
  const folderSummary = accountFolderSummary(acct);
  if (folderSummary) parts.push(folderSummary);
  if (health.reason && health.state !== 'healthy') parts.push(health.reason);
  if (!parts.length && state.unifiedMeta === null && !folderState) parts.push('awaiting local cache');
  return [...new Set(parts.filter(Boolean))].slice(0, 3).join(' · ');
}

function deriveAccountHealth(acct) {
  const accountId = acct?.id || acct?.username || '';
  const folderState = state.folderLoadState[accountId];
  const folderData = currentAccountFolderData(accountId);
  const unified = unifiedAccountSignal(acct);
  const unifiedError = unifiedAccountError(acct);
  const cockpit = state.cockpit || {};
  const authItems = cockpitAccountItems(cockpit.auth?.items, acct);
  const failedActions = cockpitAccountItems(cockpit.actions?.failed, acct);
  const watches = cockpitAccountItems(cockpit.watches?.items, acct);
  const reconnectingWatch = watches.find(item => String(item.status || '').toLowerCase() === 'reconnecting');
  const failedWatch = watches.find(item => ['failed', 'error', 'unavailable'].includes(String(item.status || '').toLowerCase()));
  let status = 'healthy';
  let reason = '';

  if (folderState === 'loading') {
    status = 'syncing';
  } else if (reconnectingWatch) {
    status = 'reconnecting';
    reason = sanitizedAccountHealthReason(reconnectingWatch.failure_reason || reconnectingWatch.status, status);
  } else if (authItems.length) {
    status = classifyAccountHealthFailure(authItems[0].reason || authItems[0].retry_guidance, 'auth_failed');
    reason = sanitizedAccountHealthReason(authItems[0].reason || authItems[0].retry_guidance, status);
  } else if (folderData?.error || folderState === 'error' || folderState === 'partial') {
    status = classifyAccountHealthFailure(folderData?.error || folderState, 'unavailable');
    reason = sanitizedAccountHealthReason(folderData?.error || folderState, status);
  } else if (unifiedError || unified?.ok === false) {
    status = classifyAccountHealthFailure(unifiedError?.error || unified?.error || unified?.freshness, 'unavailable');
    reason = sanitizedAccountHealthReason(unifiedError?.error || unified?.error || unified?.freshness, status);
  } else if (!unified && state.unifiedMeta?.status === 'error') {
    const aggregateError = (state.unifiedMeta.errors || [])[0]?.error || state.unifiedMeta.status;
    status = classifyAccountHealthFailure(aggregateError, 'unavailable');
    reason = sanitizedAccountHealthReason(aggregateError, status);
  } else if (failedWatch) {
    status = classifyAccountHealthFailure(failedWatch.failure_reason || failedWatch.status, 'unavailable');
    reason = sanitizedAccountHealthReason(failedWatch.failure_reason || failedWatch.status, status);
  } else if (failedActions.length) {
    status = classifyAccountHealthFailure(failedActions[0].justification || failedActions[0].action_taken || failedActions[0].action_status, 'unavailable');
    reason = sanitizedAccountHealthReason(failedActions[0].justification || failedActions[0].action_taken || failedActions[0].action_status, status);
  } else if (['stale', 'partial'].includes(String(unified?.freshness || '').toLowerCase())) {
    status = 'stale';
    reason = sanitizedAccountHealthReason(unified?.freshness, status);
  } else if (state.unifiedMeta === null && !folderState && !folderData) {
    status = 'syncing';
    reason = sanitizedAccountHealthReason('', status);
  }

  return {
    state: status,
    reason,
    freshness: unified?.freshness || state.unifiedMeta?.freshness || null,
    indexed_at: unified?.indexed_at || null,
    folder_state: folderState || 'not_loaded',
    folder_count: (folderData?.folders || []).length,
    provider_capabilities: accountProviderCapabilities(acct),
  };
}

function accountHealthPrimitive(acct) {
  const health = deriveAccountHealth(acct);
  const accountId = acct?.id || null;
  const label = acct?.display_name || acct?.username || accountId || 'account';
  return {
    primitive: 'account_health',
    state: {
      account_id: accountId,
      account_label: label,
      status: health.state,
      freshness: health.freshness,
      indexed_at: health.indexed_at,
      folder_state: health.folder_state,
      folder_count: health.folder_count,
      unavailable_reason: health.reason || null,
      provider_capabilities: health.provider_capabilities,
    },
    actions: {
      open: accountId ? 'available' : 'not_available',
      reconnect: 'not_available: reconnect flow is not wired yet',
      inspect_local: 'available',
    },
    audit_event: {
      event_type: 'account_health.primitive.rendered',
      account_id: accountId,
      state: health.state,
    },
    render_hint: {
      badge_class: health.state,
      status_label: health.state.replaceAll('_', ' '),
      row_markers: ['account-health-badge', 'account-health-status', 'account-sync-meta', 'account-provider-capabilities', 'account-reconnect'],
    },
    rollback_token: accountId ? `account_health:${accountId}:${health.state}` : null,
    equivalent_cli: {
      folders: `envelope folders --account ${accountCliValue(acct)} --json`,
      paths: 'envelope paths --json',
      accounts: 'envelope accounts list --json',
    },
  };
}

function messageSnippet(message) {
  return String(message?.snippet || message?.preview || message?.body_preview || '');
}

function textList(value) {
  if (Array.isArray(value)) return value.map(item => String(item || '').trim()).filter(Boolean);
  if (typeof value === 'string') return value.split(',').map(item => item.trim()).filter(Boolean);
  return [];
}

function messageLabelList(message) {
  const labels = [
    ...textList(message?.labels),
    ...textList(message?.tags),
    ...textList(message?.categories),
  ];
  const folder = messageFolder(message);
  if (folder && folder !== '__snoozed__') labels.push(displayFolderName(folder));
  if (message?.account_id && !isUnifiedView()) labels.push(messageAccountLabel(message));
  return [...new Set(labels.filter(Boolean))].slice(0, 5);
}

function messageAttachmentHint(message) {
  const attachments = Array.isArray(message?.attachments) ? message.attachments : [];
  if (attachments.length > 0) {
    const first = attachments.find(a => a?.filename) || attachments[0];
    return {
      text: attachments.length === 1 ? '1' : String(attachments.length),
      title: first?.filename ? `${attachments.length} attachment${attachments.length === 1 ? '' : 's'} · ${first.filename}` : `${attachments.length} attachment${attachments.length === 1 ? '' : 's'}`,
      present: true,
    };
  }
  const count = Number(message?.attachment_count ?? message?.attachments_count ?? 0);
  if (Number.isFinite(count) && count > 0) {
    return {
      text: String(count),
      title: `${count} attachment${count === 1 ? '' : 's'}`,
      present: true,
    };
  }
  if (message?.has_attachments === true || message?.has_attachment === true) {
    return { text: 'yes', title: 'Attachment metadata indicates attachments', present: true };
  }
  const size = Number(message?.size ?? message?.rfc822_size ?? message?.bytes ?? 0);
  if (Number.isFinite(size) && size > 0) {
    return { text: formatSize(size), title: `Message size ${formatSize(size)}`, present: false };
  }
  const contentType = message?.content_type || message?.mime_type || '';
  if (contentType) return { text: '', title: `Content type ${contentType}`, present: false };
  return { text: '', title: 'No attachment metadata', present: false };
}

function messageEquivalentCli(message) {
  const accountId = messageAccountId(message);
  const folder = messageFolder(message);
  const uid = message?.uid;
  if (!accountId || uid === undefined || uid === null || !folder) return {};
  const scoped = `${safeCliValue(uid)} --account ${safeCliValue(accountId)} --folder ${safeCliValue(folder)}`;
  return {
    read: `envelope read ${scoped} --json`,
    archive: `envelope move ${scoped} --to-folder Archive --json`,
    move: `envelope move ${scoped} --to-folder <folder> --json`,
    mark_read: `envelope flag add ${safeCliValue(uid)} \\Seen --account ${safeCliValue(accountId)} --folder ${safeCliValue(folder)} --json`,
  };
}

function messagePrimitive(message) {
  const accountId = messageAccountId(message);
  const folder = messageFolder(message);
  const uid = message?.uid ?? null;
  const unread = isMessageUnread(message);
  const attachment = messageAttachmentHint(message);
  return {
    primitive: 'message',
    state: {
      account_id: accountId,
      account_label: messageAccountLabel(message),
      folder,
      uid,
      message_id: message?.message_id || null,
      unread,
      from: message?.from_addr || '',
      subject: message?.subject || '',
      snippet: messageSnippet(message),
      date: message?.date || null,
      labels: messageLabelList(message),
      has_attachments: attachment.present,
    },
    actions: {
      open: accountId && uid !== null ? 'available' : 'not_available',
      toggle_read: accountId && uid !== null ? 'available' : 'not_available',
      bulk_archive: 'not_available',
      bulk_move: 'not_available',
      bulk_label: 'not_available',
      bulk_spam: 'not_available',
      bulk_delete: 'not_available',
    },
    audit_event: {
      event_type: 'message.primitive.rendered',
      account_id: accountId,
      folder,
      uid,
      unread,
    },
    render_hint: {
      density: 'mail-client',
      row_markers: ['msg-sender', 'msg-subject-line', 'msg-snippet', 'msg-labels', 'msg-attachment', 'msg-date'],
      attachment_hint: attachment.title,
    },
    rollback_token: accountId && folder && uid !== null ? `${accountId}:${folder}:${uid}` : null,
    equivalent_cli: messageEquivalentCli(message),
  };
}

function renderedMessagesByKey() {
  const messages = new Map();
  for (const message of state.messages) messages.set(messageKey(message), message);
  return messages;
}

function selectedVisibleKeys() {
  return state.renderedMessageKeys.filter(key => state.selectedMessages.has(key));
}

function selectedRenderedMessages() {
  const messages = renderedMessagesByKey();
  return selectedVisibleKeys().map(key => messages.get(key)).filter(Boolean);
}

function setBulkStatus(message, kind = '', { autoClear = false } = {}) {
  const status = $('bulk-status');
  if (!status) return;
  if (state.bulkStatusTimer) {
    clearTimeout(state.bulkStatusTimer);
    state.bulkStatusTimer = null;
  }
  status.textContent = message || '';
  status.className = `bulk-status ${kind}`.trim();
  // Terminal results (success/partial counts) shouldn't linger forever; in-flight
  // progress messages persist until the next setBulkStatus call replaces them.
  if (message && autoClear) {
    state.bulkStatusTimer = setTimeout(() => {
      const node = $('bulk-status');
      if (node) {
        node.textContent = '';
        node.className = 'bulk-status';
      }
      state.bulkStatusTimer = null;
    }, 6000);
  }
}

function updateBulkToolbar() {
  const toolbar = $('message-bulk-toolbar');
  const selectAll = $('select-all-messages');
  const countNode = $('selected-message-count');
  if (!toolbar || !selectAll || !countNode) return;
  const visible = state.renderedMessageKeys.length;
  const selected = selectedVisibleKeys().length;
  const actionGroup = toolbar.querySelector('.bulk-action-group');
  const primitiveLabel = toolbar.querySelector('.bulk-primitive-label');

  // Contextual affordance: toolbar collapses entirely when there are no
  // messages to act on. With messages visible but no selection, the action
  // group hides and the count cell becomes a hint.
  if (visible === 0) {
    toolbar.hidden = true;
    return;
  }
  toolbar.hidden = false;

  selectAll.checked = selected === visible;
  selectAll.indeterminate = selected > 0 && selected < visible;
  selectAll.disabled = false;

  if (selected === 0) {
    countNode.textContent = 'Select messages to enable bulk actions';
    toolbar.classList.remove('has-selection');
    if (actionGroup) actionGroup.hidden = true;
    if (primitiveLabel) primitiveLabel.hidden = true;
  } else {
    countNode.textContent = `${selected} selected`;
    toolbar.classList.add('has-selection');
    if (actionGroup) actionGroup.hidden = false;
    if (primitiveLabel) primitiveLabel.hidden = false;
  }
}

function pruneSelectedMessagesToRendered() {
  const visible = new Set(state.renderedMessageKeys);
  for (const key of Array.from(state.selectedMessages)) {
    if (!visible.has(key)) state.selectedMessages.delete(key);
  }
}

function toggleMessageSelection(message, selected, row = null) {
  const key = messageKey(message);
  if (selected) state.selectedMessages.add(key);
  else state.selectedMessages.delete(key);
  if (row) row.classList.toggle('selected', selected);
  updateBulkToolbar();
}

function toggleVisibleMessageSelection(selected) {
  for (const key of state.renderedMessageKeys) {
    if (selected) state.selectedMessages.add(key);
    else state.selectedMessages.delete(key);
  }
  renderMessages();
}

// Shift-click range select: set every row between `from` and `to` (inclusive)
// to `selected`, matching Gmail's range-toggle behavior.
function selectMessageRange(from, to, selected) {
  const keys = state.renderedMessageKeys;
  const lo = Math.max(0, Math.min(from, to));
  const hi = Math.min(keys.length - 1, Math.max(from, to));
  for (let i = lo; i <= hi; i++) {
    const key = keys[i];
    if (selected) state.selectedMessages.add(key);
    else state.selectedMessages.delete(key);
  }
  renderMessages();
}

// ── Keyboard navigation ────────────────────────────────────────────
function focusedMessageRow() {
  if (!state.focusedMessageKey) return null;
  const list = $('message-list');
  if (!list) return null;
  return Array.from(list.querySelectorAll('.msg-row'))
    .find(r => r.dataset.messageKey === state.focusedMessageKey) || null;
}

function focusedMessageObject() {
  if (!state.focusedMessageKey) return null;
  return state.messages.find(m => messageKey(m) === state.focusedMessageKey) || null;
}

function setFocusedMessageIndex(index) {
  const keys = state.renderedMessageKeys;
  if (keys.length === 0) return;
  const clamped = Math.max(0, Math.min(index, keys.length - 1));
  state.focusedMessageKey = keys[clamped];
  // Repaint focus class without a full data reload.
  const list = $('message-list');
  if (list) {
    list.querySelectorAll('.msg-row.focused').forEach(r => r.classList.remove('focused'));
    const row = Array.from(list.querySelectorAll('.msg-row'))
      .find(r => r.dataset.messageKey === state.focusedMessageKey);
    if (row) {
      row.classList.add('focused');
      row.scrollIntoView({ block: 'nearest' });
    }
  }
}

function moveMessageFocus(delta) {
  const keys = state.renderedMessageKeys;
  if (keys.length === 0) return;
  const current = state.focusedMessageKey ? keys.indexOf(state.focusedMessageKey) : -1;
  const next = current < 0 ? (delta > 0 ? 0 : keys.length - 1) : current + delta;
  setFocusedMessageIndex(next);
}

function openFocusedMessage() {
  const m = focusedMessageObject();
  if (!m) return;
  const p = messagePrimitive(m).state;
  openMessage(p.uid, p.account_id, p.folder);
}

function toggleFocusedSelection() {
  const m = focusedMessageObject();
  if (!m) return;
  const key = messageKey(m);
  const selected = !state.selectedMessages.has(key);
  if (selected) state.selectedMessages.add(key);
  else state.selectedMessages.delete(key);
  state.lastSelectedKey = key;
  // Single repaint — updateBulkToolbar runs inside renderMessages.
  renderMessages();
}

function starFocusedMessage() {
  const m = focusedMessageObject();
  if (!m) return;
  const row = focusedMessageRow();
  toggleMessageStar(m, row ? row.querySelector('.msg-star') : null);
}

// Single source of truth for shortcuts — also rendered in the cheat sheet.
const KEYBOARD_SHORTCUTS = [
  { keys: 'j / k', label: 'Next / previous message' },
  { keys: 'Enter / o', label: 'Open focused message' },
  { keys: 'x', label: 'Select / deselect focused' },
  { keys: 's', label: 'Star / unstar focused' },
  { keys: 'e', label: 'Archive selected' },
  { keys: '#', label: 'Delete selected' },
  { keys: 'r', label: 'Reply to open message' },
  { keys: 'a', label: 'Reply all to open message' },
  { keys: 'c', label: 'Compose new message' },
  { keys: '/', label: 'Focus search' },
  { keys: 'Esc', label: 'Close reader / modal' },
  { keys: '?', label: 'Toggle this shortcut sheet' },
];

function isTextEntryFocused() {
  const a = document.activeElement;
  if (!a) return false;
  const tag = a.tagName;
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || a.isContentEditable;
}

function anyModalOpen() {
  return ['composer', 'add-account-modal', 'snooze-modal', 'shortcut-sheet']
    .some(id => $(id)?.classList.contains('show'));
}

function toggleShortcutSheet(force) {
  const sheet = $('shortcut-sheet');
  if (!sheet) return;
  const show = force === undefined ? !sheet.classList.contains('show') : force;
  if (show && !sheet.dataset.populated) {
    const list = $('shortcut-sheet-list');
    if (list) {
      clear(list);
      for (const s of KEYBOARD_SHORTCUTS) {
        const row = el('div', { class: 'shortcut-row' });
        row.appendChild(el('kbd', { class: 'shortcut-key', text: s.keys }));
        row.appendChild(el('span', { class: 'shortcut-label', text: s.label }));
        list.appendChild(row);
      }
    }
    sheet.dataset.populated = '1';
  }
  sheet.classList.toggle('show', show);
}

function handleGlobalKeydown(event) {
  // ? toggles the cheat sheet even when nothing is focused.
  if (event.key === '?' && !isTextEntryFocused()) {
    event.preventDefault();
    toggleShortcutSheet();
    return;
  }
  // Escape always closes the topmost surface.
  if (event.key === 'Escape') {
    if ($('shortcut-sheet')?.classList.contains('show')) { toggleShortcutSheet(false); return; }
    if ($('composer')?.classList.contains('show')) { closeComposer(); return; }
    if ($('snooze-modal')?.classList.contains('show')) { closeSnoozeModal(); return; }
    if ($('add-account-modal')?.classList.contains('show')) { closeAddAccount(); return; }
    if ($('reader')?.classList.contains('show')) { closeReader(); return; }
    return;
  }
  // All other shortcuts are suppressed while typing or in a modal.
  if (isTextEntryFocused() || anyModalOpen()) return;
  if (event.metaKey || event.ctrlKey || event.altKey) return;

  switch (event.key) {
    case 'j': event.preventDefault(); moveMessageFocus(1); break;
    case 'k': event.preventDefault(); moveMessageFocus(-1); break;
    case 'o': case 'Enter': event.preventDefault(); openFocusedMessage(); break;
    case 'x': event.preventDefault(); toggleFocusedSelection(); break;
    case 's': event.preventDefault(); starFocusedMessage(); break;
    case 'e': event.preventDefault(); if (selectedRenderedMessages().length) bulkArchive(); break;
    case '#': event.preventDefault(); if (selectedRenderedMessages().length) bulkDelete(); break;
    case 'r': event.preventDefault(); if (state.currentMessage) openComposer('reply', state.currentMessage); break;
    case 'a': event.preventDefault(); if (state.currentMessage) openComposer('reply-all', state.currentMessage); break;
    case 'c': event.preventDefault(); openComposer('new'); break;
    case '/': event.preventDefault(); $('search-input')?.focus(); break;
    default: break;
  }
}

function isMessageStarred(message) {
  if (!message) return false;
  if (state.starredMessages.has(messageKey(message))) return true;
  return Array.isArray(message.flags)
    && message.flags.some(f => String(f).toLowerCase() === '\\flagged');
}

async function toggleMessageStar(message, button) {
  const key = messageKey(message);
  const wasStarred = isMessageStarred(message);
  const accountId = messageAccountId(message);
  const folder = messageFolder(message) || 'INBOX';
  const uid = Number(message?.uid);
  if (!accountId || !uid || folder === '__snoozed__') {
    toast('Cannot star — message has no account/uid context.', 'error');
    return;
  }

  // Optimistic update: flip UI state immediately, revert on API failure.
  if (wasStarred) state.starredMessages.delete(key);
  else state.starredMessages.add(key);
  button.textContent = wasStarred ? '☆' : '★';
  button.classList.toggle('active', !wasStarred);
  button.setAttribute('aria-pressed', wasStarred ? 'false' : 'true');
  button.disabled = true;

  try {
    await api('POST', `/accounts/${accountId}/messages/${uid}/flags`, {
      folder,
      add: wasStarred ? [] : ['\\Flagged'],
      remove: wasStarred ? ['\\Flagged'] : [],
    });
    // Mirror to message.flags so subsequent renders stay correct.
    const flags = Array.isArray(message.flags) ? message.flags.slice() : [];
    const filtered = flags.filter(f => String(f).toLowerCase() !== '\\flagged');
    message.flags = wasStarred ? filtered : [...filtered, '\\Flagged'];
    setBulkStatus(wasStarred ? 'Star removed.' : 'Starred.', 'success');
  } catch (e) {
    // Revert optimistic update on failure.
    if (wasStarred) state.starredMessages.add(key);
    else state.starredMessages.delete(key);
    button.textContent = wasStarred ? '★' : '☆';
    button.classList.toggle('active', wasStarred);
    button.setAttribute('aria-pressed', wasStarred ? 'true' : 'false');
    toast(`Star failed: ${e.message}`, 'error');
  } finally {
    button.disabled = false;
  }
}

function reportBulkActionUnavailable(action) {
  const selected = selectedVisibleKeys().length;
  if (selected === 0) {
    setBulkStatus('Select messages to enable bulk actions.');
    return;
  }
  const niceAction = action.charAt(0).toUpperCase() + action.slice(1);
  setBulkStatus(`${niceAction} on ${selected} message${selected === 1 ? '' : 's'} — bulk actions aren't available yet.`, 'pending');
  toast(`Bulk ${action} isn't available yet; act on one message at a time for now.`, 'pending');
}

// Resolve canonical folder names (Archive, Trash, Spam) from the loaded
// folder list for an account. Returns the first matching name or null.
const FOLDER_CANDIDATES = {
  archive: ['Archive', 'All Mail', '[Gmail]/All Mail', 'INBOX.Archive', 'Archives'],
  trash: ['Trash', 'Bin', 'Deleted', 'Deleted Messages', 'Deleted Items', '[Gmail]/Trash', 'INBOX.Trash'],
  spam: ['Spam', 'Junk', 'Junk Mail', 'Junk E-mail', '[Gmail]/Spam', 'INBOX.Spam', 'INBOX.Junk'],
};

function detectAccountFolder(accountId, canonical) {
  const data = currentAccountFolderData(accountId);
  const folderNames = new Set((data?.folders || []).map(f => f.name || f));
  for (const candidate of (FOLDER_CANDIDATES[canonical] || [])) {
    if (folderNames.has(candidate)) return candidate;
  }
  return null;
}

// Run a bulk per-message operation with bounded concurrency and per-row
// progress reporting. `perItem(message)` must return a Promise that
// resolves on success / rejects on failure. The caller is responsible
// for triggering a list refresh after a successful batch.
async function runBulkOp({ label, perItem, concurrency = 4 }) {
  const messages = selectedRenderedMessages();
  if (messages.length === 0) {
    setBulkStatus('Select messages to enable bulk actions.');
    return { ok: 0, failed: [] };
  }
  setBulkStatus(`${label}: 0/${messages.length}`, 'pending');
  let okCount = 0;
  const failed = [];
  let index = 0;

  const worker = async () => {
    while (index < messages.length) {
      const i = index++;
      const m = messages[i];
      try {
        await perItem(m);
        okCount += 1;
      } catch (e) {
        failed.push({ message: m, error: e.message || String(e) });
      }
      setBulkStatus(
        `${label}: ${okCount + failed.length}/${messages.length}${failed.length ? ` · ${failed.length} failed` : ''}`,
        failed.length ? 'warning' : 'pending',
      );
    }
  };

  const workers = Array.from({ length: Math.min(concurrency, messages.length) }, worker);
  await Promise.all(workers);

  const summary = failed.length === 0
    ? `${label}: all ${okCount} message${okCount === 1 ? '' : 's'} succeeded.`
    : `${label}: ${okCount} succeeded, ${failed.length} failed.`;
  setBulkStatus(summary, failed.length ? 'warning' : 'success', { autoClear: true });
  toast(summary, failed.length ? 'error' : 'success');
  if (failed.length) {
    for (const item of failed.slice(0, 3)) {
      console.error(`[bulk ${label}] uid=${item.message?.uid} folder=${messageFolder(item.message)}: ${item.error}`);
    }
  }
  return { ok: okCount, failed };
}

async function bulkDelete() {
  const messages = selectedRenderedMessages();
  if (messages.length === 0) {
    setBulkStatus('Select messages to enable bulk actions.');
    return;
  }
  const result = await runBulkOp({
    label: 'Delete',
    perItem: async (m) => {
      const accountId = messageAccountId(m);
      const folder = messageFolder(m) || 'INBOX';
      const uid = Number(m?.uid);
      if (!accountId || !uid) throw new Error('missing account/uid');
      await api('DELETE', `/accounts/${accountId}/messages/${uid}?folder=${encodeURIComponent(folder)}`);
    },
  });
  // Refresh list so the deleted rows disappear
  if (result.ok > 0) {
    state.selectedMessages.clear();
    if (isUnifiedView()) await loadUnifiedInbox({ refresh: true });
    else await loadMessages();
  }
}

async function bulkArchive() {
  const messages = selectedRenderedMessages();
  if (messages.length === 0) {
    setBulkStatus('Select messages to enable bulk actions.');
    return;
  }
  // Group by account so we can detect each account's Archive folder once
  const byAccount = new Map();
  for (const m of messages) {
    const acct = messageAccountId(m);
    if (!acct) continue;
    if (!byAccount.has(acct)) byAccount.set(acct, []);
    byAccount.get(acct).push(m);
  }
  // Pre-resolve archive folder per account, fail-fast if any account has none
  const archiveFolderByAccount = new Map();
  const missing = [];
  for (const acct of byAccount.keys()) {
    const target = detectAccountFolder(acct, 'archive');
    if (!target) missing.push(acct);
    else archiveFolderByAccount.set(acct, target);
  }
  if (missing.length) {
    const msg = `No Archive/All Mail folder found for ${missing.length} account${missing.length === 1 ? '' : 's'}. Refresh folders first.`;
    setBulkStatus(msg, 'warning');
    toast(msg, 'error');
    return;
  }
  const result = await runBulkOp({
    label: 'Archive',
    perItem: async (m) => {
      const accountId = messageAccountId(m);
      const folder = messageFolder(m) || 'INBOX';
      const uid = Number(m?.uid);
      const toFolder = archiveFolderByAccount.get(accountId);
      if (!accountId || !uid || !toFolder) throw new Error('missing account/uid/archive');
      await api('POST', `/accounts/${accountId}/messages/${uid}/move`, {
        folder,
        to_folder: toFolder,
      });
    },
  });
  if (result.ok > 0) {
    state.selectedMessages.clear();
    if (isUnifiedView()) await loadUnifiedInbox({ refresh: true });
    else await loadMessages();
  }
}

function selectedPrimitiveCliPacket() {
  const primitives = selectedRenderedMessages().map(messagePrimitive);
  if (primitives.length === 0) return '';
  return JSON.stringify({
    selected_message_primitives: primitives.map(primitive => ({
      primitive: primitive.primitive,
      state: {
        account_id: primitive.state.account_id,
        folder: primitive.state.folder,
        uid: primitive.state.uid,
        subject: primitive.state.subject,
      },
      rollback_token: primitive.rollback_token,
      equivalent_cli: primitive.equivalent_cli,
    })),
  }, null, 2);
}

async function copySelectedEquivalentCli() {
  const packet = selectedPrimitiveCliPacket();
  if (!packet) {
    setBulkStatus('Select messages first');
    return;
  }
  try {
    if (!navigator.clipboard?.writeText) throw new Error('clipboard unavailable');
    await navigator.clipboard.writeText(packet);
    setBulkStatus(`Copied CLI packet for ${selectedVisibleKeys().length} selected message primitives.`, 'success');
    toast('Copied CLI packet', 'success');
  } catch (_) {
    setBulkStatus(packet, 'warning');
    toast(packet, 'error');
  }
}

function wireBulkToolbar() {
  const selectAll = $('select-all-messages');
  if (!selectAll) return;
  selectAll.onchange = (event) => toggleVisibleMessageSelection(event.target.checked);

  // Wired bulk actions (Phase 1.3 + 1.4): real backend mutations via
  // existing per-message endpoints with bounded concurrency.
  const archive = $('bulk-archive');
  if (archive) archive.onclick = bulkArchive;
  const del = $('bulk-delete');
  if (del) del.onclick = bulkDelete;

  // Honestly-stubbed actions until v0.11 ships the folder picker /
  // label input / spam-rule wiring. These announce when they'll arrive,
  // never claim to mutate mail.
  for (const [id, action] of [
    ['bulk-move', 'move'],
    ['bulk-label', 'label'],
    ['bulk-spam', 'spam'],
  ]) {
    const button = $(id);
    if (button) button.onclick = () => reportBulkActionUnavailable(action);
  }

  const copy = $('copy-equivalent-cli');
  if (copy) copy.onclick = copySelectedEquivalentCli;
  updateBulkToolbar();
}

// ── Agent Cockpit ──────────────────────────────────────────────────
function cockpitPath() { return state.currentAccount ? `/accounts/${state.currentAccount.id}/cockpit` : '/cockpit'; }
function setCockpitExpanded(expanded) {
  state.cockpitExpanded = expanded;
  const panel = $('cockpit-panel');
  const button = $('btn-toggle-cockpit');
  panel.hidden = !expanded;
  button.setAttribute('aria-expanded', expanded ? 'true' : 'false');
  button.textContent = expanded ? 'Collapse' : 'Expand';
}
function setCockpitLoading(message = 'Loading cockpit…') {
  $('cockpit-account').textContent = state.currentAccount ? state.currentAccount.username : 'all accounts';
  const summary = $('cockpit-summary'); clear(summary); summary.appendChild(el('div', { class: 'cockpit-loading', text: message }));
  for (const id of ['cockpit-watches', 'cockpit-events', 'cockpit-drafts', 'cockpit-errors', 'cockpit-rules', 'cockpit-snoozes']) clear($(id));
}
async function loadCockpit() {
  setCockpitLoading();
  try { state.cockpit = await api('GET', cockpitPath()); renderCockpit(); }
  catch (e) { state.cockpit = null; setCockpitLoading('Cockpit unavailable. Backend returned: ' + e.message); toast('Cockpit: ' + e.message, 'error'); }
}
function renderCockpit() {
  const data = state.cockpit || {}; const summary = data.summary || {}; $('cockpit-account').textContent = state.currentAccount ? state.currentAccount.username : 'all accounts';
  const summaryNode = $('cockpit-summary'); clear(summaryNode);
  for (const [label, value, meta] of [['Watches', summary.watches?.count ?? 0, summary.watches?.status === 'available' ? 'active' : 'unavailable'], ['Operator events', summary.recent_events ?? 0, `${summary.audit_events ?? 0} audit hidden`], ['Pending drafts', summary.pending_drafts ?? 0, (summary.pending_drafts || 0) > 0 ? 'needs operator' : 'clear'], ['Failed actions', summary.failed_actions ?? 0, (summary.failed_actions || 0) > 0 ? 'review' : 'clear'], ['Rule runs', summary.recent_rule_runs ?? 0, `${summary.enabled_rules ?? 0} enabled`], ['Due snoozes', summary.due_snoozes ?? 0, (summary.due_snoozes || 0) > 0 ? 'due now' : 'clear']]) {
    const card = el('div', { class: 'cockpit-summary-card' }); card.appendChild(el('span', { class: 'cockpit-summary-label', text: label })); card.appendChild(el('strong', { text: String(value) })); card.appendChild(el('span', { class: 'cockpit-summary-meta', text: meta })); summaryNode.appendChild(card);
  }
  $('cockpit-watches-status').textContent = data.watches?.status || 'unknown'; $('cockpit-events-count').textContent = `${summary.needs_attention_events ?? 0} attention`; $('cockpit-drafts-count').textContent = String((data.drafts?.pending || []).length); $('cockpit-errors-count').textContent = String((data.actions?.failed || []).length + ((data.auth?.items || []).length)); $('cockpit-rules-count').textContent = String((data.rules?.recent_runs || []).length); $('cockpit-snoozes-count').textContent = String((data.snoozes?.due || []).length);
  renderCockpitWatches(data.watches || {}); renderCockpitEvents(data.events || {}); renderCockpitDrafts(data.drafts || {}); renderCockpitErrors(data.auth || {}, data.actions?.failed || []); renderCockpitRules(data.rules || {}, summary); renderCockpitSnoozes(data.snoozes?.due || []);
  renderSmartMailboxes();
}
function renderCockpitWatches(watches) { const list = $('cockpit-watches'); clear(list); const items = watches?.items || []; if (!items.length) { list.appendChild(el('div', { class: 'cockpit-empty', text: 'No persistent watches registered for this scope.' })); return; } for (const item of items.slice(0, 6)) list.appendChild(cockpitRow(`${item.account_id || 'account'} · ${item.folder || 'folder ?'}`, `${item.status || 'unknown'} · ${item.last_heartbeat_at || item.updated_at || ''}`, item.failure_reason || (item.last_event_at ? `last event ${item.last_event_at}` : item.schedule || ''), item.status === 'failed' ? 'error' : '')); }
function cockpitRow(primary, meta, detail, kind = '') { const row = el('div', { class: `cockpit-row ${kind}` }); row.appendChild(el('div', { class: 'cockpit-row-primary', text: primary || '(untitled)' })); if (meta) row.appendChild(el('div', { class: 'cockpit-row-meta', text: meta })); if (detail) row.appendChild(el('div', { class: 'cockpit-row-detail', text: detail })); return row; }
function eventAge(createdAt) { const then = Date.parse(createdAt || ''); if (Number.isNaN(then)) return createdAt || 'age ?'; const seconds = Math.max(0, Math.floor((Date.now() - then) / 1000)); if (seconds < 90) return `${seconds}s ago`; const minutes = Math.floor(seconds / 60); if (minutes < 90) return `${minutes}m ago`; const hours = Math.floor(minutes / 60); if (hours < 48) return `${hours}h ago`; return `${Math.floor(hours / 24)}d ago`; }
function eventPerson(evt, key) { return evt[key] || evt.payload?.[key] || ''; }
function cockpitEventRow(evt) { const title = evt.subject || evt.event_type || '(event)'; const actor = evt.actor || evt.source || 'source ?'; const folder = evt.folder ? ` · ${evt.folder}` : ''; const uid = evt.uid ? ` · UID ${evt.uid}` : ''; const meta = `${evt.account_label || evt.account_id || 'account'} · ${actor} · ${evt.event_type || 'event'} → ${evt.outcome || 'recorded'}${folder}${uid} · ${eventAge(evt.created_at)} · ${evt.ack_state || (evt.acked_at ? 'acked' : 'pending')}`; const people = [eventPerson(evt, 'from_addr') && `from ${eventPerson(evt, 'from_addr')}`, eventPerson(evt, 'to_addr') && `to ${eventPerson(evt, 'to_addr')}`].filter(Boolean).join(' · '); const detailText = [people, evt.snippet].filter(Boolean).join(' — '); const row = cockpitRow(title, meta, detailText, evt.ack_state === 'pending' || evt.secure_pending ? 'pending' : ''); if (evt.message_link) row.appendChild(el('a', { class: 'cockpit-row-link', text: 'Open message', href: evt.message_link })); return row; }
function renderCockpitEventBucket(list, label, events, emptyText) { list.appendChild(el('div', { class: 'cockpit-bucket-label', text: label })); if (!events.length) { list.appendChild(el('div', { class: 'cockpit-empty', text: emptyText })); return; } for (const evt of events.slice(0, 4)) list.appendChild(cockpitEventRow(evt)); }
function renderCockpitEvents(events) { const list = $('cockpit-events'); clear(list); const needs = events.needs_attention || []; const mailbox = events.mailbox || []; const agentActions = events.agent_actions || []; renderCockpitEventBucket(list, 'Needs attention', needs, 'Nothing needs acknowledgment. Review pending drafts or inspect failures if you expected work.'); renderCockpitEventBucket(list, 'Mailbox/watch events', mailbox, 'No mailbox/watch events. Create an OTP watch or start a mailbox watcher to populate this bucket.'); renderCockpitEventBucket(list, 'Recent agent actions', agentActions, 'No recent agent actions. Draft approvals, rule runs, and non-routine send-policy outcomes will appear here.'); if ((events.audit || []).length) list.appendChild(el('div', { class: 'cockpit-note', text: `${events.audit.length} routine audit event${events.audit.length === 1 ? '' : 's'} hidden from Cockpit. Use the Audit Log/debug filter for raw telemetry.` })); }
function renderCockpitDrafts(drafts) {
  const list = $('cockpit-drafts'); clear(list); const pending = drafts.pending || [];
  const unavailableReason = drafts.unavailable_reason && drafts.unavailable_reason !== null ? String(drafts.unavailable_reason).replaceAll('_', ' ') : '';
  const actionsUnavailable = drafts.actions?.send === 'not_available';
  if (!pending.length) {
    const counts = drafts.counts || {};
    const detail = actionsUnavailable ? `Draft actions unavailable: ${unavailableReason || 'not wired for this scope'}.` : `${counts.draft || 0} regular draft${counts.draft === 1 ? '' : 's'}.`;
    list.appendChild(el('div', { class: 'cockpit-empty', text: `No pending review drafts. ${detail}` }));
  } else {
    const actionText = drafts.actions?.send === 'available_confirm_required' ? 'Actions available: approve, edit, discard, block; send requires explicit confirm.' : `Draft actions unavailable: ${unavailableReason || 'not wired for this scope'}.`;
    for (const draft of pending.slice(0, 5)) list.appendChild(cockpitRow(draft.subject || '(no subject)', `${draft.status} · to ${draft.to_addr || '?'}`, draft.status === 'blocked' ? 'Blocked draft — inspect before sending.' : actionText, draft.status === 'blocked' ? 'error' : 'pending'));
  }
}
function renderCockpitErrors(auth, failedActions) { const list = $('cockpit-errors'); clear(list); const authItems = auth.items || []; if (!authItems.length && !failedActions.length) { list.appendChild(el('div', { class: 'cockpit-empty', text: 'No failed auth records or failed actions reported.' })); return; } for (const item of authItems.slice(0, 4)) list.appendChild(cockpitRow(`${item.account_id || 'account'} · ${item.backend || 'auth'}`, item.created_at || '', item.reason || item.retry_guidance || '', 'error')); for (const action of failedActions.slice(0, 4)) list.appendChild(cockpitRow(action.action_type, `${action.action_status} · ${action.created_at || ''}`, action.justification || action.action_taken || '', 'error')); }
function renderCockpitRules(rules, summary) { const list = $('cockpit-rules'); clear(list); const runs = rules.recent_runs || []; if (!runs.length) { list.appendChild(el('div', { class: 'cockpit-empty', text: `No recent rule-run audit records. ${summary.enabled_rules || 0}/${summary.rules || 0} rules enabled.` })); return; } for (const run of runs.slice(0, 5)) list.appendChild(cockpitRow(run.rule_name || run.rule_id || 'rule run', `${run.status} · ${run.created_at || ''}`, run.action || run.error || (run.uid ? `UID ${run.uid}` : ''))); }
function renderCockpitSnoozes(snoozes) { const list = $('cockpit-snoozes'); clear(list); if (!snoozes.length) { list.appendChild(el('div', { class: 'cockpit-empty', text: 'No snoozes due now.' })); return; } for (const item of snoozes.slice(0, 6)) list.appendChild(cockpitRow(item.subject || '(no subject)', `due ${item.return_at} · ${item.original_folder || 'folder ?'}`, item.reason || item.note || 'Ready for unsnooze sweep.', 'pending')); }

// ── Rules ──────────────────────────────────────────────────────────
async function loadRules() {
  const summary = $('rules-summary');
  const list = $('rules-list');
  clear(list);
  if (!state.currentAccount) {
    summary.textContent = 'select an account';
    state.rules = [];
    $('btn-run-rules').disabled = true;
    return;
  }
  summary.textContent = 'loading rules…';
  try {
    const data = await api('GET', `/accounts/${state.currentAccount.id}/rules`);
    state.rules = data.rules || [];
    renderRules();
  } catch (e) {
    state.rules = [];
    summary.textContent = 'rules unavailable';
    toast('Rules: ' + e.message, 'error');
  }
}

function renderRules() {
  const summary = $('rules-summary');
  const list = $('rules-list');
  clear(list);
  const enabled = state.rules.filter(r => r.enabled).length;
  summary.textContent = `${state.rules.length} rule${state.rules.length === 1 ? '' : 's'} · ${enabled} enabled`;
  $('btn-run-rules').disabled = !state.currentAccount || enabled === 0;

  const cli = el('button', { class: 'btn-ghost text-xs w-full', text: 'Copy equivalent CLI' });
  cli.onclick = () => copyText(currentRulesCli());
  list.appendChild(cli);

  if (state.rules.length === 0) {
    list.appendChild(el('div', { class: 'rules-empty', text: 'No rules yet. Create from CLI today; visual builder is next.' }));
    return;
  }

  for (const rule of state.rules) {
    const risk = ruleActionRisk(rule.action);
    const card = el('div', { class: `rule-card ${rule.enabled ? 'enabled' : 'disabled'} ${risk}` });
    card.appendChild(el('div', { class: 'rule-title', text: rule.name || '(unnamed rule)', title: rule.id || '' }));
    card.appendChild(el('div', { class: 'rule-meta', text: `${ruleAccountText(rule)} · priority ${rule.priority} · ${rule.enabled ? 'enabled' : 'disabled/proposed'} · ${rule.stop ? 'stop' : 'continue'} · ${rule.hit_count || 0} hits` }));
    card.appendChild(el('div', { class: `rule-action ${risk}`, text: `${ruleActionText(rule.action)} · ${risk}`, title: rule.action || '' }));
    card.appendChild(el('div', { class: 'rule-match', text: ruleMatchText(rule.match_expr), title: rule.match_expr || '' }));
    const previewPanel = el('div', { class: 'rule-preview-panel' });
    previewPanel.appendChild(el('div', { class: 'rule-preview-status', text: 'Preview not run · mutated=false' }));
    const previewButton = el('button', { class: 'btn-ghost text-xs', text: rule.preview?.status === 'complete' ? 'Refresh preview' : 'Preview blast radius' });
    previewButton.onclick = () => previewRuleBlastRadius(rule, previewPanel, previewButton);
    card.appendChild(previewButton);
    card.appendChild(previewPanel);
    list.appendChild(card);
  }
}

async function previewRuleBlastRadius(rule, panel, button) {
  if (!state.currentAccount || !rule?.id) return;
  const limit = boundedRulesLimit();
  button.disabled = true;
  clear(panel);
  panel.appendChild(el('div', { class: 'rule-preview-status', text: `previewing ${displayFolderName(state.currentFolder)} · limit ${limit} · mutated=false…` }));
  try {
    const result = await api('POST', `/accounts/${state.currentAccount.id}/rules/${encodeURIComponent(rule.id)}/preview`, {
      folder: state.currentFolder,
      limit,
    });
    rule.preview = Object.assign({ status: 'complete' }, result);
    renderRulePreviewResult(panel, rule.preview);
  } catch (e) {
    clear(panel);
    panel.appendChild(el('div', { class: 'rule-preview-status error', text: `preview failed · mutated=false · ${e.message}` }));
  } finally {
    button.disabled = false;
  }
}

function renderRulePreviewResult(panel, preview) {
  clear(panel);
  const processed = preview.processed || 0;
  const matched = preview.matched || 0;
  const samples = preview.samples || [];
  panel.appendChild(el('div', { class: 'rule-preview-status', text: `${preview.folder || state.currentFolder} · processed ${processed}/${preview.limit || processed} · matched ${matched} · unread ${preview.unread_matched || 0} · mutated=false` }));
  if (!samples.length) {
    panel.appendChild(el('div', { class: 'rule-preview-empty', text: 'No sampled messages would be touched by this rule.' }));
    return;
  }
  for (const sample of samples) {
    const row = el('div', { class: 'rule-preview-sample' });
    row.appendChild(el('span', { class: 'rule-preview-uid', text: `UID ${sample.uid}` }));
    row.appendChild(el('span', { class: 'rule-preview-from', text: sample.from || '(unknown)' }));
    row.appendChild(el('span', { class: 'rule-preview-subject', text: sample.subject || '(no subject)' }));
    if (sample.message_link) row.appendChild(el('a', { class: 'preview-sample-link', text: 'Open', href: sample.message_link }));
    panel.appendChild(row);
  }
}

function currentRulesRunCli(limit) {
  if (!state.currentAccount) return '';
  const account = state.currentAccount.username || state.currentAccount.id;
  return `envelope rule run --account ${account} --folder ${state.currentFolder} --limit ${limit} --confirm --json`;
}

function boundedRulesLimit() {
  const input = $('rules-run-limit');
  const raw = Number.parseInt(input.value, 10);
  const limit = Number.isFinite(raw) ? Math.max(1, Math.min(200, raw)) : 50;
  input.value = String(limit);
  return limit;
}

async function runEnabledRulesForCurrentFolder() {
  if (!state.currentAccount) {
    toast('Select an account first', 'error');
    return;
  }
  if (state.currentFolder === '__snoozed__') {
    toast('Rules run against real IMAP folders, not Snoozed', 'error');
    return;
  }
  const enabled = state.rules.filter(r => r.enabled).length;
  if (enabled === 0) {
    toast('No enabled rules for this account', 'error');
    return;
  }
  const limit = boundedRulesLimit();
  const folderLabel = displayFolderName(state.currentFolder);
  const highRisk = state.rules.some(r => r.enabled && ruleActionRisk(r.action) === 'high-risk');
  const warning = highRisk ? '\n\nHIGH-RISK enabled action present (delete/trash/junk). Confirm only after previewing blast radius.' : '';
  if (!confirm(`Live run will mutate mailbox. Account: ${state.currentAccount.username || state.currentAccount.id}. Folder: ${folderLabel}. Limit: ${limit}. Enabled rules: ${enabled}.${warning}`)) return;

  $('btn-run-rules').disabled = true;
  $('rules-run-status').textContent = `running ${enabled} rule${enabled === 1 ? '' : 's'} on ${folderLabel}…`;
  const log = $('rules-run-log');
  clear(log);
  log.classList.add('hidden');

  try {
    const result = await api('POST', `/accounts/${state.currentAccount.id}/rules/run`, {
      folder: state.currentFolder,
      limit,
      confirm: true,
    });
    renderRuleRunResult(result, limit);
    await loadRules();
    await loadMessages();
    await loadFolders();
  } catch (e) {
    $('rules-run-status').textContent = 'rule run failed';
    toast('Rule run: ' + e.message, 'error');
  } finally {
    $('btn-run-rules').disabled = !state.currentAccount || state.rules.filter(r => r.enabled).length === 0;
  }
}

function renderRuleRunResult(result, limit) {
  const processed = result.processed || 0;
  const actions = result.actions || 0;
  $('rules-run-status').textContent = `processed ${processed}/${limit} · ${actions} action${actions === 1 ? '' : 's'}`;
  const log = $('rules-run-log');
  clear(log);
  const entries = (result.log || []).slice(0, 8);
  if (entries.length === 0) {
    log.appendChild(el('div', { class: 'rule-run-empty', text: result.message || 'No rules matched.' }));
  } else {
    for (const entry of entries) {
      const line = entry.status === 'ok'
        ? `UID ${entry.uid}: ${entry.rule} → ${entry.action}`
        : `UID ${entry.uid}: ${entry.rule} failed — ${entry.error}`;
      log.appendChild(el('div', { class: `rule-run-line ${entry.status === 'ok' ? 'ok' : 'error'}`, text: line }));
    }
    if ((result.log || []).length > entries.length) {
      log.appendChild(el('div', { class: 'rule-run-more', text: `+ ${(result.log || []).length - entries.length} more` }));
    }
  }
  log.classList.remove('hidden');
  toast(`Rules run complete: ${actions} action${actions === 1 ? '' : 's'}`, actions ? 'success' : '');
}

async function testRulesForCurrentMessage() {
  const accountId = messageAccountId();
  const folder = messageFolder();
  if (!accountId || !state.currentMessage) return;
  const panel = $('reader-rules-panel');
  clear(panel);
  panel.classList.remove('hidden');
  panel.appendChild(el('div', { class: 'text-xs font-mono text-mid', text: 'Dry-running enabled rules…' }));
  try {
    const data = await api(
      'GET',
      `/accounts/${accountId}/rules/test/${state.currentMessage.uid}?folder=${encodeURIComponent(folder)}`
    );
    renderRuleTestResult(data);
  } catch (e) {
    clear(panel);
    panel.appendChild(el('div', { class: 'text-xs font-mono text-warn', text: 'Rule dry-run failed: ' + e.message }));
  }
}

function renderRuleTestResult(data) {
  const panel = $('reader-rules-panel');
  clear(panel);
  const matches = data.matches || [];
  panel.appendChild(el('p', { class: 'section-label mb-2', text: 'Rule dry-run' }));
  panel.appendChild(el('div', {
    class: 'text-xs font-mono text-mid mb-2',
    text: `${data.rules_evaluated || 0} enabled rule${data.rules_evaluated === 1 ? '' : 's'} evaluated · ${matches.length} match${matches.length === 1 ? '' : 'es'}`,
  }));
  if (matches.length === 0) {
    panel.appendChild(el('div', { class: 'rule-test-empty', text: 'No rules would touch this message.' }));
    return;
  }
  for (const match of matches) {
    const row = el('div', { class: 'rule-test-match' });
    row.appendChild(el('span', { class: 'rule-test-name', text: match.rule_name || '(unnamed rule)' }));
    row.appendChild(el('span', { class: 'rule-test-action', text: ruleActionText(match.action) + (match.stop ? ' · stop' : '') }));
    panel.appendChild(row);
  }
}

function clearRuleTestPanel() {
  const panel = $('reader-rules-panel');
  clear(panel);
  panel.classList.add('hidden');
}

// ── Stats ──────────────────────────────────────────────────────────
async function loadStats() {
  try {
    const stats = await api('GET', '/stats');
    state.stats = stats || {};
    setStat('stat-accounts', stats.accounts ?? 0);
    setStat('stat-snoozed', stats.snoozed ?? 0);
    setStat('stat-drafts', stats.drafts ?? 0);
    renderSmartMailboxes();
  } catch (e) {
    setStat('stat-accounts', 'error');
    setStat('stat-snoozed', 'error');
    setStat('stat-drafts', 'error');
    console.error('loadStats', e);
  }
}

function renderMailboxNotice(title, message, kind = 'not_available') {
  $('list-title').textContent = title;
  // Notice box below holds the message; keep the count cell clean.
  $('list-count').textContent = '';
  state.messages = [];
  const list = $('message-list');
  clear(list);
  // 'pending' (coming-soon) and 'empty' are neutral tones; only true
  // 'error'/'not_available' should look warning-colored.
  const tone = (kind === 'loading' || kind === 'empty' || kind === 'pending') ? 'text-mid' : 'text-warn';
  const box = el('div', { class: `px-4 py-12 text-center text-sm ${tone}` });
  box.appendChild(el('div', { class: 'font-semibold mb-1', text: title }));
  box.appendChild(el('div', { class: 'font-mono text-xs', text: message }));
  list.appendChild(box);
}

function smartMailboxCount(key) {
  if (key === 'unified') {
    if (state.unifiedMeta?.unread_count !== undefined) {
      return state.unifiedMeta.unread_count > 0 ? `${state.unifiedMeta.unread_count} unread` : '';
    }
    return '';
  }
  if (key === 'attention') {
    const count = state.cockpit?.summary?.needs_attention_events;
    if (count === undefined || count === 0) return '';
    return `${count} attention`;
  }
  if (key === 'snoozed') {
    const n = state.stats?.snoozed;
    if (n === undefined || n === 0) return '';
    return String(n);
  }
  const account = selectedAccountForSmartMailbox();
  if (!account) return '';
  const folder = findFolderForSmart(key);
  if (!folder) return '';
  return folderCountText(folder);
}

function mailboxItem({ key, label, count, hint, active, unavailable, onclick }) {
  const item = el('button', {
    class: `folder-item ${active ? 'active' : ''} ${unavailable ? 'not-available' : ''}`,
    type: 'button',
    title: hint || label,
    onclick,
    aria: { current: active ? 'page' : 'false' },
  });
  const labelWrap = el('span', { class: 'mailbox-label' });
  labelWrap.appendChild(el('span', { class: 'name', text: label }));
  if (hint) labelWrap.appendChild(el('span', { class: 'mailbox-kicker', text: hint }));
  item.appendChild(labelWrap);
  item.appendChild(el('span', { class: 'count', text: count || '' }));
  return item;
}

function renderSmartMailboxes() {
  const list = $('folder-list');
  if (!list) return;
  clear(list);
  for (const mailbox of SMART_MAILBOXES) {
    list.appendChild(mailboxItem({
      key: mailbox.key,
      label: mailbox.label,
      hint: mailbox.hint,
      count: smartMailboxCount(mailbox.key),
      active: state.currentSmartMailbox === mailbox.key,
      unavailable: false,
      onclick: () => selectSmartMailbox(mailbox.key),
    }));
  }
}

function sortedFolders(folders) {
  return [...(folders || [])].sort((a, b) => {
    if (a.folder === 'INBOX') return -1;
    if (b.folder === 'INBOX') return 1;
    return displayFolderName(a.folder).localeCompare(displayFolderName(b.folder));
  });
}

function currentAccountFolderData(accountId) {
  return state.accountFolders[accountId] || null;
}

function renderAccountMailboxButtons(container, acct, data) {
  const folders = sortedFolders(data?.folders || []);
  for (const f of folders) {
    const count = f.unseen && f.unseen > 0 ? `${f.unseen} unread / ${f.exists}` : `${f.exists}`;
    container.appendChild(mailboxItem({
      key: f.folder,
      label: displayFolderName(f.folder),
      count,
      hint: f.folder,
      active: state.currentAccount?.id === acct.id && state.currentFolder === f.folder,
      onclick: () => selectAccount(acct, f.folder),
    }));
  }

  if (data?.snoozed_virtual) {
    container.appendChild(mailboxItem({
      key: '__snoozed__',
      label: 'Snoozed',
      count: String(data.snoozed_virtual.exists || 0),
      hint: 'local snooze ledger',
      active: state.currentAccount?.id === acct.id && state.currentFolder === '__snoozed__',
      onclick: () => selectAccount(acct, '__snoozed__'),
    }));
  }
}

function renderReaderEmpty(message = 'Select a message to read. Opening a message does not mark it read.') {
  $('reader-subject').textContent = 'No message selected';
  $('reader-from').textContent = '';
  $('reader-to').textContent = '';
  $('reader-date').textContent = '';
  $('reader-read-state').textContent = '';
  $('reader-meta').classList.add('hidden');
  $('reader-account-row').classList.add('hidden');
  $('reader-thread-row').classList.add('hidden');
  $('reader-cc-row').classList.add('hidden');
  clearRuleTestPanel();
  const body = $('reader-body');
  clear(body);
  body.appendChild(el('div', { class: 'text-center text-mid py-12 font-mono text-xs', text: message }));
  clear($('reader-attachments'));
  $('reader-attachments').classList.add('hidden');
}

// ── Accounts ───────────────────────────────────────────────────────
async function loadAccounts({ autoSelect = true } = {}) {
  try {
    const data = await api('GET', '/accounts');
    state.accounts = data.accounts || [];
    const currentStillExists = state.currentAccount && state.accounts.some(a => a.id === state.currentAccount.id);
    if (!currentStillExists) state.currentAccount = null;
    if (state.accounts.length === 0) {
      state.currentView = 'empty';
      state.currentSmartMailbox = null;
      state.cockpitStatus = 'no accounts';
      state.cockpitMessage = { kind: 'error', text: 'No accounts are configured in this dashboard.' };
      renderAccountSwitcher();
      renderAccountsList();
      renderSmartMailboxes();
      renderDrafts();
      renderMailboxNotice('No accounts configured', 'Add an account to load mailboxes. First paint does not probe IMAP credentials.', 'empty');
      return;
    }
    // Deep-link routing: if a route was parsed from the URL, resolve the
    // account slug and bail out gracefully when no account matches.
    const routeAccount = state.route ? resolveAccountSlug(state.route.accountSlug || state.route.accountId) : null;
    if (state.route && !routeAccount) {
      state.cockpitStatus = 'deep link account not found';
      state.cockpitMessage = {
        kind: 'error',
        text: `No dashboard account matched "${state.route.accountSlug}".`,
      };
      renderAccountSwitcher();
      renderAccountsList();
      renderDrafts();
      if (typeof focusAgentCockpit === 'function') focusAgentCockpit();
      toast('Deep link account not found', 'error');
      return;
    }
    if (!autoSelect) {
      renderAccountSwitcher();
      renderAccountsList();
      renderSmartMailboxes();
      return;
    }
    if (routeAccount) {
      await selectAccount(routeAccount);
      if (typeof applyDashboardRoute === 'function') await applyDashboardRoute();
    } else {
      await selectUnifiedInbox();
    }
  } catch (e) {
    toast('Failed to load accounts: ' + e.message, 'error');
  }
}

function renderAccountSwitcher() {
  const sel = $('account-switcher');
  clear(sel);
  const unified = el('option', { text: 'Unified Inbox' });
  unified.value = '__unified__';
  unified.selected = isUnifiedView();
  sel.appendChild(unified);
  for (const acct of state.accounts) {
    const opt = el('option', { text: acct.username });
    opt.value = acct.id;
    if (!isUnifiedView() && state.currentAccount && state.currentAccount.id === acct.id) opt.selected = true;
    sel.appendChild(opt);
  }
}

// States that mean "something needs the operator's attention." A healthy
// or in-flight account has no remediation action to expose.
const HEALTH_NEEDS_ACTION = new Set(['auth_failed', 'unavailable', 'stale', 'reconnecting']);

function renderAccountHealthPanel(acct, primitive) {
  const status = primitive.state.status;
  const panel = el('div', { class: `account-health-panel ${status}` });
  const syncText = accountSyncMeta(acct, {
    state: status,
    reason: primitive.state.unavailable_reason,
  });
  panel.appendChild(el('div', {
    class: 'account-sync-meta',
    text: syncText || 'local signals unavailable',
    title: syncText || 'local account health signals unavailable',
  }));
  panel.appendChild(el('div', {
    class: 'account-provider-capabilities',
    text: primitive.state.provider_capabilities || 'capabilities unknown',
    title: primitive.state.provider_capabilities || 'capabilities unknown',
  }));
  // Contextual affordance: an action button only renders when the account
  // needs operator attention. Healthy / syncing accounts show nothing.
  // stale    → cached index is old but auth is fine; Refresh reloads folders
  //            and messages without touching credentials.
  // all others (auth_failed, unavailable, reconnecting) → Reconnect runs the
  //            /verify probe to re-check IMAP credentials.
  if (HEALTH_NEEDS_ACTION.has(status)) {
    const actionRow = el('div', { class: 'account-health-actions' });
    const actionStatus = el('span', { class: 'account-reconnect-status' });

    if (status === 'stale') {
      const refreshBtn = el('button', {
        class: 'account-reconnect',
        text: 'Refresh',
        title: 'Reload folder list and messages to clear the stale cache',
        type: 'button',
      });
      refreshBtn.onclick = async (event) => {
        event.stopPropagation();
        refreshBtn.disabled = true;
        const originalText = refreshBtn.textContent;
        refreshBtn.textContent = 'Refreshing…';
        actionStatus.textContent = '';
        try {
          await selectAccount(acct);
          actionStatus.textContent = 'Refreshed ✓';
          toast(`Refreshed ${acct.username}.`, 'success');
        } catch (e) {
          actionStatus.textContent = e.message || 'refresh failed';
          toast(`Refresh error: ${e.message}`, 'error');
        } finally {
          refreshBtn.disabled = false;
          refreshBtn.textContent = originalText;
        }
      };
      actionRow.appendChild(refreshBtn);
    } else {
      const reconnect = el('button', {
        class: 'account-reconnect',
        text: 'Reconnect',
        title: 'Re-verify IMAP credentials and clear the failed-auth record',
        type: 'button',
      });
      reconnect.onclick = async (event) => {
        event.stopPropagation();
        reconnect.disabled = true;
        const originalText = reconnect.textContent;
        reconnect.textContent = 'Verifying…';
        actionStatus.textContent = '';
        try {
          const result = await api('POST', `/accounts/${acct.id}/verify`);
          if (result && result.ok && result.imap) {
            actionStatus.textContent = 'IMAP verified ✓';
            toast(`Verified ${acct.username}.`, 'success');
            // Refresh accounts + cockpit so the health badge updates
            // immediately. autoSelect=false keeps the user's current view.
            await loadAccounts({ autoSelect: false });
            await loadCockpit();
          } else {
            const msg = (result && result.error) || 'Verify failed';
            actionStatus.textContent = msg;
            toast(`Verify failed: ${msg}`, 'error');
          }
        } catch (e) {
          actionStatus.textContent = e.message || 'request failed';
          toast(`Reconnect error: ${e.message}`, 'error');
        } finally {
          reconnect.disabled = false;
          reconnect.textContent = originalText;
        }
      };
      actionRow.appendChild(reconnect);
    }

    actionRow.appendChild(actionStatus);
    panel.appendChild(actionRow);
  }
  return panel;
}

function renderAccountsList() {
  const list = $('accounts-list');
  clear(list);
  if (state.accounts.length === 0) {
    list.appendChild(el('div', { class: 'account-empty', text: 'No accounts configured.' }));
    return;
  }
  const COLLAPSED_LIMIT = 3;
  const showAll = state.accounts.length <= COLLAPSED_LIMIT || state.showAllAccounts;
  const visible = showAll ? state.accounts : state.accounts.slice(0, COLLAPSED_LIMIT);

  for (const acct of visible) {
    const primitive = accountHealthPrimitive(acct);
    const status = primitive.state.status;
    const group = el('div', { class: 'account-group' });
    const selectBtn = el('button', { class: 'account-select', title: acct.username, type: 'button' });
    selectBtn.appendChild(el('span', { class: 'account-name', text: acct.username }));
    const badge = el('span', {
      class: `account-health-badge ${status}`,
      title: primitive.state.unavailable_reason || primitive.render_hint.status_label,
    });
    badge.appendChild(el('span', {
      class: 'account-health-status',
      text: primitive.render_hint.status_label,
    }));
    selectBtn.appendChild(badge);
    selectBtn.onclick = () => selectAccount(acct);
    const delBtn = el('button', { class: 'account-delete', text: '×', title: 'Delete account', type: 'button' });
    delBtn.onclick = async () => {
      if (!confirm(`Delete account ${acct.username}?`)) return;
      try {
        await api('DELETE', `/accounts/${acct.id}`);
        toast('Account deleted', 'success');
        await loadAccounts();
        await loadStats();
      } catch (e) { toast('Delete failed: ' + e.message, 'error'); }
    };
    const item = el('div', {
      class: `account-item ${!isUnifiedView() && state.currentAccount?.id === acct.id ? 'active' : ''}`,
      data: { primitive: primitive.primitive },
    }, [selectBtn, delBtn]);
    item.setAttribute('data-primitive', 'account_health');
    item.accountHealthPrimitive = primitive;
    group.appendChild(item);
    group.appendChild(renderAccountHealthPanel(acct, primitive));

    const folderBox = el('div', { class: 'account-mailboxes' });
    const data = currentAccountFolderData(acct.id);
    if (state.folderLoadState[acct.id] === 'loading') {
      folderBox.appendChild(el('div', { class: 'account-mailbox-empty', text: 'Loading mailboxes...' }));
    } else if (data && ((data.folders || []).length || data.snoozed_virtual)) {
      renderAccountMailboxButtons(folderBox, acct, data);
    } else if (state.currentAccount?.id === acct.id && state.folderLoadState[acct.id] === 'error') {
      folderBox.appendChild(el('div', { class: 'account-mailbox-empty', text: 'Mailboxes unavailable. Refresh or verify credentials.' }));
    } else {
      folderBox.appendChild(el('div', { class: 'account-mailbox-empty', text: 'Select account to load mailboxes.' }));
    }
    group.appendChild(folderBox);
    list.appendChild(group);
  }

  if (!showAll && state.accounts.length > COLLAPSED_LIMIT) {
    const more = el('button', {
      class: 'account-more',
      text: `+ ${state.accounts.length - COLLAPSED_LIMIT} more`,
      type: 'button',
    });
    more.onclick = () => { state.showAllAccounts = true; renderAccountsList(); };
    list.appendChild(more);
  } else if (state.accounts.length > COLLAPSED_LIMIT) {
    const less = el('button', {
      class: 'account-more',
      text: 'show less',
      type: 'button',
    });
    less.onclick = () => { state.showAllAccounts = false; renderAccountsList(); };
    list.appendChild(less);
  }
}

async function selectUnifiedInbox({ refresh = false } = {}) {
  state.currentView = 'unified';
  state.currentSmartMailbox = 'unified';
  state.currentAccount = null;
  state.currentFolder = 'INBOX';
  state.currentDraft = null;
  state.cockpitMessage = null;
  state.folders = [];
  state.unifiedMeta = null;
  setSearchState('');
  $('search-input').value = '';
  renderAccountSwitcher();
  renderAccountsList();
  renderSmartMailboxes();
  await loadRules();
  await loadUnifiedInbox({ refresh });
  loadCockpit();
  scheduleAutoRefresh(90_000);
}

async function selectSmartMailbox(key) {
  if (key === 'unified') return selectUnifiedInbox();

  state.currentSmartMailbox = key;
  renderSmartMailboxes();

  if (key === 'attention') {
    state.currentView = 'attention';
    renderMailboxNotice('Needs Attention', 'Items needing your eyes appear in the Cockpit strip above. A dedicated message queue is planned.', 'pending');
    if (!state.cockpit) loadCockpit();
    return;
  }

  if (key === 'snoozed') {
    const account = selectedAccountForSmartMailbox();
    if (!account) {
      renderMailboxNotice('Snoozed', 'Pick an account from the sidebar to see its snoozed messages. Aggregate snoozed view is coming soon.', 'pending');
      return;
    }
    return selectAccount(account, '__snoozed__', { smartMailbox: 'snoozed' });
  }

  if (SMART_FOLDER_DEFAULTS[key]) {
    const account = selectedAccountForSmartMailbox();
    if (!account) {
      renderMailboxNotice(SMART_MAILBOXES.find(m => m.key === key)?.label || 'Mailbox', 'Pick an account from the sidebar to open this mailbox.', 'pending');
      return;
    }
    return openAccountSmartFolder(account, key);
  }
}

async function openAccountSmartFolder(acct, key) {
  state.currentView = 'account';
  state.currentSmartMailbox = key;
  state.currentAccount = acct;
  state.currentFolder = SMART_FOLDER_DEFAULTS[key] || 'INBOX';
  state.unifiedMeta = null;
  setSearchState('');
  $('search-input').value = '';
  renderAccountSwitcher();
  renderAccountsList();
  renderSmartMailboxes();
  await loadRules();
  await loadFolders();
  state.currentFolder = folderNameForSmart(key);
  await loadMessages();
  await loadDrafts();
  renderSmartMailboxes();
  renderAccountsList();
  loadCockpit();
}

async function selectAccount(acct, folder = 'INBOX', options = {}) {
  const {
    loadFolderList = true,
    loadMessageList = true,
    loadCockpitPanel = true,
    smartMailbox = null,
  } = options;
  state.currentView = 'account';
  state.currentSmartMailbox = smartMailbox;
  state.currentAccount = acct;
  state.currentFolder = folder || 'INBOX';
  state.unifiedMeta = null;
  setSearchState('');
  $('search-input').value = '';
  renderAccountSwitcher();
  renderAccountsList();
  renderSmartMailboxes();
  await loadRules();
  if (loadFolderList) {
    await loadFolders();
  } else {
    renderAccountsList();
  }
  if (loadMessageList) {
    await loadMessages();
  } else {
    renderMailboxNotice(
      displayFolderName(state.currentFolder),
      'Account selected. Choose a mailbox to load live messages.',
      'empty'
    );
  }
  if (loadCockpitPanel) loadCockpit();
  scheduleAutoRefresh(120_000);
}

// ── Folders ────────────────────────────────────────────────────────
async function loadFolders() {
  if (isUnifiedView()) {
    renderSmartMailboxes();
    renderAccountsList();
    const unread = state.unifiedMeta?.unread_count;
    setStat('stat-unread', unread === undefined ? '—' : String(unread));
    return;
  }
  if (!state.currentAccount) return;
  setRefresh('loading folders…');
  setStat('stat-unread', 'loading');
  state.folderLoadState[state.currentAccount.id] = 'loading';
  renderAccountsList();
  try {
    const data = await api('GET', `/accounts/${state.currentAccount.id}/folders`);
    state.folders = data.folders || [];
    state.accountFolders[state.currentAccount.id] = data;
    state.folderLoadState[state.currentAccount.id] = data.error ? 'partial' : 'loaded';
    if (data.error) {
      // Partial success — server returned snoozed but IMAP failed
      toast('Folders: ' + data.error, 'error');
      setRefresh('partial');
    } else {
      setRefresh('ok');
    }
    renderFolders(data);
    updateCurrentFolderStats();
  } catch (e) {
    state.folderLoadState[state.currentAccount.id] = 'error';
    renderAccountsList();
    setRefresh('error');
    setStat('stat-unread', 'error');
    toast('Folders: ' + e.message, 'error');
  }
}

function renderUnifiedFolders() {
  renderSmartMailboxes();
  renderAccountsList();
}

function renderFolders(data) {
  if (state.currentAccount) {
    state.accountFolders[state.currentAccount.id] = data;
  }
  renderSmartMailboxes();
  renderAccountsList();
}

// ── Messages list ──────────────────────────────────────────────────
async function loadMessages() {
  if (isUnifiedView()) return loadUnifiedInbox();
  if (!state.currentAccount) return;
  const acctLabel = state.currentAccount ? state.currentAccount.username : '';
  const folderLabel = state.currentFolder === '__snoozed__' ? '★ Snoozed' : displayFolderName(state.currentFolder);
  $('list-title').textContent = acctLabel ? `${folderLabel} — ${acctLabel}` : folderLabel;
  if (state.currentFolder === '__snoozed__') return loadSnoozed();

  setRefresh('loading messages…');
  // Show loading state immediately
  const list = $('message-list');
  clear(list);
  list.appendChild(el('div', { class: 'px-4 py-12 text-center text-sm text-mid', text: 'Loading messages…' }));
  $('list-count').textContent = '';
  try {
    const data = await api(
      'GET',
      `/accounts/${state.currentAccount.id}/messages?folder=${encodeURIComponent(state.currentFolder)}&limit=50`
    );
    state.messages = data.messages || [];
    renderMessages();
    const meta = folderMeta(state.currentFolder);
    const total = meta ? ` / ${folderCountText(meta)}` : '';
    $('list-count').textContent = `Showing ${state.messages.length} latest${total}`;
    setRefresh('ok');
  } catch (e) {
    clear(list);
    list.appendChild(el('div', { class: 'px-4 py-12 text-center text-sm text-warn', text: 'Failed to load messages' }));
    setRefresh('error');
    toast('Messages: ' + e.message, 'error');
  }
}

async function loadUnifiedInbox({ refresh = false } = {}) {
  state.currentFolder = 'INBOX';
  $('list-title').textContent = 'Unified Inbox';
  setRefresh(refresh ? 'refreshing unified inbox…' : 'loading unified cache…');
  const list = $('message-list');
  clear(list);
  list.appendChild(el('div', { class: 'px-4 py-12 text-center text-sm text-mid', text: refresh ? 'Refreshing unified inbox from mailboxes…' : 'Loading cached unified inbox…' }));
  $('list-count').textContent = '';
  try {
    const data = await api(refresh ? 'POST' : 'GET', refresh ? '/messages/unified/refresh?limit=50' : '/messages/unified?limit=50');
    state.unifiedMeta = data;
    state.messages = data.messages || [];
    renderMessages();
    const accounts = data.accounts || [];
    const okCount = accounts.filter(a => a.ok).length;
    const errors = data.errors || [];
    const allCacheMissing = errors.length > 0
      && errors.every(e => /cache missing|refresh required/i.test(e.error || ''));
    const accountText = accounts.length ? ` across ${okCount}/${accounts.length} account${accounts.length === 1 ? '' : 's'}` : '';
    // Don't shout "all accounts failed" when it's just an unwarmed cache.
    const statusText = allCacheMissing
      ? ''
      : (data.status === 'partial' ? ' · partial' : data.status === 'error' ? ' · some accounts failed' : '');
    const unreadText = data.unread_count !== undefined ? ` · ${data.unread_count} unread` : '';
    if (allCacheMissing) {
      // Notice box below already shows the CTA; keep the count line blank.
      $('list-count').textContent = '';
    } else {
      $('list-count').textContent = `Showing ${state.messages.length} latest${accountText}${unreadText}${statusText}`;
    }
    setStat('stat-unread', data.unread_count === undefined ? '—' : String(data.unread_count));
    renderUnifiedFolders();
    setRefresh(data.status === 'partial' ? 'partial' : data.status === 'error' ? 'error' : 'ok');
  } catch (e) {
    state.unifiedMeta = { status: 'error', errors: [{ error: e.message }] };
    state.messages = [];
    renderMessages();
    $('list-count').textContent = 'Unified Inbox unavailable';
    renderUnifiedFolders();
    setRefresh('error');
    toast('Unified Inbox: ' + e.message, 'error');
  }
}

function renderMessages() {
  const list = $('message-list');
  clear(list);
  const unified = isUnifiedView();
  const meta = unified ? (state.unifiedMeta || {}) : {};
  const errors = meta.errors || [];
  if (unified && errors.length > 0) {
    appendUnifiedErrorNotice(list, meta.status, errors);
  }
  if (state.messages.length === 0) {
    state.renderedMessageKeys = [];
    pruneSelectedMessagesToRendered();
    updateBulkToolbar();
    // Only show an empty-state placeholder when there isn't already a
    // notice rendered above (cache_missing CTA, partial failure list, etc.).
    if (!unified || (errors.length === 0 && meta.status !== 'error')) {
      const emptyText = unified ? 'No messages in Unified Inbox.' : 'No messages.';
      list.appendChild(el('div', { class: 'px-4 py-12 text-center text-sm text-mid', text: emptyText }));
    }
    return;
  }
  const sorted = unified ? [...state.messages] : [...state.messages].sort((a, b) => (b.uid || 0) - (a.uid || 0));
  state.renderedMessageKeys = sorted.map(messageKey);
  pruneSelectedMessagesToRendered();
  if (state.focusedMessageKey && !state.renderedMessageKeys.includes(state.focusedMessageKey)) {
    state.focusedMessageKey = null;
  }
  let renderIndex = -1;
  for (const m of sorted) {
    const rowIndex = ++renderIndex;
    const unread = isMessageUnread(m);
    const primitive = messagePrimitive(m);
    const key = messageKey(m);
    const selected = state.selectedMessages.has(key);
    const starred = isMessageStarred(m);
    const row = el('div', {
      class: `msg-row ${unified ? 'unified' : ''}`,
      data: { primitive: primitive.primitive, messageKey: key },
    });
    row.setAttribute('data-primitive', 'message');
    row.dataset.rowIndex = String(rowIndex);
    row.messagePrimitive = primitive;
    if (unread) row.classList.add('unseen');
    if (selected) row.classList.add('selected');
    if (key === state.focusedMessageKey) row.classList.add('focused');

    const controls = el('div', { class: 'msg-controls' });
    controls.onclick = (event) => event.stopPropagation();

    const selectBox = document.createElement('input');
    selectBox.type = 'checkbox';
    selectBox.className = 'msg-select';
    selectBox.checked = selected;
    selectBox.setAttribute('aria-label', `Select ${primitive.state.subject || 'message'}`);
    // Capture shift state on click (onchange doesn't carry modifier keys).
    // The anchor is stored as a message KEY, not a raw index, so it survives
    // list re-sorts/re-renders — resolved to a live index at click time.
    let rangeExtend = false;
    selectBox.onclick = (event) => {
      event.stopPropagation();
      rangeExtend = event.shiftKey
        && state.lastSelectedKey != null
        && state.renderedMessageKeys.includes(state.lastSelectedKey);
    };
    selectBox.onchange = (event) => {
      event.stopPropagation();
      const checked = event.target.checked;
      const anchorIndex = state.lastSelectedKey != null
        ? state.renderedMessageKeys.indexOf(state.lastSelectedKey)
        : -1;
      if (rangeExtend && anchorIndex >= 0) {
        selectMessageRange(anchorIndex, rowIndex, checked);
      } else {
        toggleMessageSelection(m, checked, row);
      }
      state.lastSelectedKey = key;
    };
    controls.appendChild(selectBox);

    const star = el('button', {
      class: `msg-star ${starred ? 'active' : ''}`,
      text: starred ? '★' : '☆',
      title: starred ? 'Unstar (removes IMAP \\Flagged)' : 'Star (sets IMAP \\Flagged)',
      type: 'button',
    });
    star.setAttribute('aria-label', starred ? 'Unstar message' : 'Star message');
    star.setAttribute('aria-pressed', starred ? 'true' : 'false');
    star.onclick = (event) => {
      event.stopPropagation();
      toggleMessageStar(m, star);
    };
    controls.appendChild(star);

    const readToggle = el('button', {
      class: `read-toggle ${unread ? 'unread' : 'read'}`,
      text: unread ? '●' : '○',
      title: unread ? 'Unread — click to mark read' : 'Read — click to mark unread',
    });
    readToggle.onclick = (event) => {
      event.stopPropagation();
      toggleMessageReadState(m);
    };
    controls.appendChild(readToggle);
    row.appendChild(controls);

    row.appendChild(el('div', {
      class: 'msg-sender from',
      text: primitive.state.from || '(unknown)',
      title: primitive.state.from || '',
    }));

    const subjectCell = el('div', { class: 'subject' });
    subjectCell.appendChild(el('div', {
      class: 'msg-subject-line subject-line',
      text: primitive.state.subject || '(no subject)',
      title: primitive.state.subject || '',
    }));
    subjectCell.appendChild(el('div', {
      class: `msg-snippet ${primitive.state.snippet ? '' : 'unavailable'}`,
      text: primitive.state.snippet,
      title: primitive.state.snippet || 'No snippet metadata available',
    }));
    const threadText = threadMetaText(m);
    if (threadText) subjectCell.appendChild(el('div', { class: 'thread-meta', text: threadText, title: threadText }));
    row.appendChild(subjectCell);

    const labelsCell = el('div', { class: 'msg-labels' });
    for (const label of primitive.state.labels) {
      labelsCell.appendChild(el('span', { class: 'msg-label', text: label, title: label }));
    }
    if (unified) {
      const accountScope = `${messageAccountLabel(m)} · ${m.folder || 'INBOX'}`;
      labelsCell.appendChild(el('span', { class: 'msg-label account-scope', text: accountScope, title: accountScope }));
    }
    row.appendChild(labelsCell);

    const attachment = messageAttachmentHint(m);
    row.appendChild(el('div', {
      class: `msg-attachment ${attachment.present ? 'has-attachment' : ''}`,
      text: attachment.text,
      title: attachment.title,
      aria: { label: attachment.title },
    }));
    row.appendChild(el('div', { class: 'msg-date date', text: formatDate(primitive.state.date) }));
    row.onclick = () => openMessage(primitive.state.uid, primitive.state.account_id, primitive.state.folder);
    list.appendChild(row);
  }
  updateBulkToolbar();
}

function appendUnifiedErrorNotice(list, status, errors) {
  // When every account has the same cache-missing problem, collapse the
  // 17-line red wall into one calm CTA. This isn't a failure — it's an
  // empty cache that needs a refresh.
  const allCacheMissing = errors.length > 0
    && errors.every(err => /cache missing|refresh required/i.test(err.error || ''));
  if (allCacheMissing) {
    const box = el('div', { class: 'message-notice pending' });
    box.appendChild(el('div', { class: 'message-notice-title', text: 'Mailbox cache is empty.' }));
    const action = el('div', { class: 'message-notice-line' });
    const refresh = el('button', {
      class: 'btn-ghost text-xs',
      text: 'Refresh now',
      type: 'button',
    });
    refresh.onclick = () => loadUnifiedInbox({ refresh: true });
    action.appendChild(document.createTextNode('Click '));
    action.appendChild(refresh);
    action.appendChild(document.createTextNode(' to populate the unified inbox across all accounts.'));
    box.appendChild(action);
    list.appendChild(box);
    return;
  }

  // Otherwise show the per-account failure breakdown.
  const title = status === 'error'
    ? 'Some inboxes failed to load.'
    : 'Some accounts failed to load.';
  const box = el('div', { class: `message-notice ${status === 'error' ? 'error' : 'partial'}` });
  box.appendChild(el('div', { class: 'message-notice-title', text: title }));
  for (const err of errors) {
    box.appendChild(el('div', {
      class: 'message-notice-line',
      text: `${accountDisplay(err.account_id, err.account_username)} · ${err.folder || 'INBOX'}: ${err.error || 'unknown error'}`,
    }));
  }
  list.appendChild(box);
}

function formatDate(iso) {
  if (!iso) return '';
  const d = new Date(iso);
  if (isNaN(d.getTime())) return String(iso).slice(0, 10);
  const now = new Date();
  if (d.toDateString() === now.toDateString()) return d.toTimeString().slice(0, 5);
  return d.toISOString().slice(0, 10);
}

// ── Message reader ─────────────────────────────────────────────────
async function openMessage(uid, accountId = null, folder = null) {
  const effectiveAccountId = accountId || state.currentAccount?.id;
  const effectiveFolder = folder || state.currentFolder || 'INBOX';
  if (!effectiveAccountId) return;
  // Drafts-folder links should not depend on the read-message endpoint. Local
  // Envelope drafts may exist even when the backing IMAP message is not cached,
  // and the desired destination is the editor, not the reader.
  if (folderMatchesSmart(effectiveFolder, 'drafts')) {
    await openDraftFromMessageLink(effectiveAccountId, uid, effectiveFolder, null);
    return;
  }
  // Show reader immediately with loading state
  $('reader-subject').textContent = 'Loading…';
  $('reader-from').textContent = '';
  $('reader-to').textContent = '';
  $('reader-date').textContent = '';
  $('reader-account-row').classList.add('hidden');
  $('reader-account').textContent = '';
  $('reader-read-state').textContent = '';
  $('reader-thread-row').classList.add('hidden');
  $('reader-thread').textContent = '';
  $('reader-thread-link').removeAttribute('href');
  clear($('reader-body'));
  clearRuleTestPanel();
  $('reader-body').appendChild(el('div', { class: 'text-center text-mid py-12', text: 'Loading message…' }));
  $('reader').classList.add('show');
  try {
    const data = await api(
      'GET',
      `/accounts/${effectiveAccountId}/messages/${uid}?folder=${encodeURIComponent(effectiveFolder)}`
    );
    const account = accountById(effectiveAccountId);
    state.currentMessage = Object.assign({}, data.message, {
      account_id: effectiveAccountId,
      account_username: account?.username || accountId || '',
      account_display_name: account?.display_name || null,
      folder: effectiveFolder,
    });
    state.loadRemoteImages = false;
    renderReader();
  } catch (e) {
    $('reader-subject').textContent = 'Error';
    clear($('reader-body'));
    $('reader-body').appendChild(el('div', { class: 'text-center text-warn py-12', text: e.message }));
  }
}

function normalizeContentId(value) {
  return String(value || '')
    .trim()
    .replace(/^cid:/i, '')
    .replace(/^<|>$/g, '')
    .trim()
    .toLowerCase();
}

function attachmentUrl(msg, attachment, inline = false) {
  const accountId = messageAccountId(msg);
  const folder = messageFolder(msg);
  const filename = attachment?.filename || '';
  const inlineParam = inline ? '&inline=true' : '';
  return `/api/accounts/${encodeURIComponent(accountId)}/messages/${msg.uid}/attachments/${encodeURIComponent(filename)}?folder=${encodeURIComponent(folder)}${inlineParam}`;
}

function isInlineAttachment(attachment) {
  const contentId = normalizeContentId(attachment?.content_id);
  return Boolean(contentId) && String(attachment?.content_type || '').toLowerCase().startsWith('image/');
}

function cidAttachmentMap(msg) {
  const map = new Map();
  for (const attachment of msg?.attachments || []) {
    const key = normalizeContentId(attachment.content_id);
    if (key && attachment.filename && isInlineAttachment(attachment)) map.set(key, attachment);
  }
  return map;
}

function transparentImageDataUrl() {
  return 'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 width=%221%22 height=%221%22/%3E';
}

function stripDangerousEmailNodes(doc) {
  doc.querySelectorAll('script, style, link, form, input, button, textarea, select, iframe, object, embed, applet, meta, base, svg, image').forEach(node => node.remove());
  doc.querySelectorAll('*').forEach(node => {
    for (const attr of Array.from(node.attributes || [])) {
      const name = attr.name.toLowerCase();
      const value = attr.value || '';
      const tag = node.tagName.toLowerCase();
      if (name.startsWith('on')) node.removeAttribute(attr.name);
      if (name === 'background') node.removeAttribute(attr.name);
      if (name === 'srcset') node.removeAttribute(attr.name);
      if (name === 'poster' && isBlockedEmailUrl(value)) node.removeAttribute(attr.name);
      if ((name === 'href' || name === 'xlink:href') && isDangerousNavigationUrl(value)) node.removeAttribute(attr.name);
      if ((name === 'href' || name === 'xlink:href') && (name === 'xlink:href' || tag === 'image' || tag === 'use') && isBlockedEmailUrl(value)) node.removeAttribute(attr.name);
      if (name === 'src' && tag !== 'img' && isBlockedEmailUrl(value)) node.removeAttribute(attr.name);
      if (name === 'style' && hasCssUrlLoad(value)) node.removeAttribute(attr.name);
    }
  });
}

function isRemoteHttpUrl(value) {
  return /^https?:\/\//i.test(String(value || '').trim());
}

function isProtocolRelativeUrl(value) {
  return /^\/\//.test(String(value || '').trim());
}

function isDangerousNavigationUrl(value) {
  const trimmed = String(value || '').trim();
  return /^javascript:/i.test(trimmed) || /^data:text\/html/i.test(trimmed);
}

function isBlockedEmailUrl(value) {
  const trimmed = String(value || '').trim();
  return isDangerousNavigationUrl(trimmed)
    || isProtocolRelativeUrl(trimmed)
    || isRemoteHttpUrl(trimmed);
}

function hasCssUrlLoad(value) {
  const css = String(value || '');
  return /@import\b/i.test(css) || /url\s*\(/i.test(css);
}

// Detect whether there is any meaningful (non-whitespace) content preceding a
// node, walking previous siblings up the ancestor chain to <body>. Used to
// avoid collapsing a message whose entire body IS the quote (nothing would be
// left visible) — we only collapse quotes that follow a real reply.
function hasMeaningfulContentBefore(node) {
  let cur = node;
  while (cur && cur.parentNode) {
    let sib = cur.previousSibling;
    while (sib) {
      if (sib.nodeType === 3 && (sib.textContent || '').replace(/\s+/g, '').length > 0) return true;
      if (sib.nodeType === 1) {
        if ((sib.textContent || '').replace(/\s+/g, '').length > 0) return true;
        if (sib.querySelector && sib.querySelector('img')) return true;
      }
      sib = sib.previousSibling;
    }
    cur = cur.parentNode;
    if (cur && cur.tagName && cur.tagName.toLowerCase() === 'body') break;
  }
  return false;
}

// Wrap quoted-reply blocks in native <details> so they collapse by default and
// expand on click WITHOUT any script execution inside the email iframe. The
// parent re-measures on the native `toggle` event (see attachQuoteToggleRemeasure).
function collapseQuotedReplies(doc) {
  const body = doc.body;
  if (!body) return 0;
  const candidates = Array.from(
    body.querySelectorAll('blockquote, .gmail_quote, div[class*="gmail_quote"]'),
  );
  let wrapped = 0;
  for (const node of candidates) {
    if (!node.parentNode) continue;
    // Skip nested quotes — only wrap the outermost quote block.
    if (candidates.some(other => other !== node && other.contains(node))) continue;
    if (!hasMeaningfulContentBefore(node)) continue;
    const details = doc.createElement('details');
    details.className = 'envelope-quote';
    const summary = doc.createElement('summary');
    summary.className = 'envelope-quote-toggle';
    summary.textContent = 'Show quoted text';
    node.parentNode.insertBefore(details, node);
    details.appendChild(summary);
    details.appendChild(node);
    wrapped += 1;
  }
  return wrapped;
}

function sanitizeEmailHtml(html, msg, loadRemoteImages = false) {
  const parser = new DOMParser();
  const doc = parser.parseFromString(String(html || ''), 'text/html');
  stripDangerousEmailNodes(doc);
  const cidMap = cidAttachmentMap(msg);
  let remoteBlocked = 0;

  doc.querySelectorAll('img').forEach(img => {
    const rawSrc = img.getAttribute('src') || '';
    const trimmed = rawSrc.trim();
    img.removeAttribute('srcset');
    if (/^cid:/i.test(trimmed)) {
      const attachment = cidMap.get(normalizeContentId(trimmed));
      if (attachment) {
        img.setAttribute('src', attachmentUrl(msg, attachment, true));
        img.setAttribute('data-envelope-inline', normalizeContentId(trimmed));
      } else {
        img.setAttribute('alt', img.getAttribute('alt') || 'Missing inline image');
        img.removeAttribute('src');
      }
      return;
    }
    if (isRemoteHttpUrl(trimmed)) {
      if (!loadRemoteImages) {
        img.setAttribute('data-remote-src', trimmed);
        img.setAttribute('src', transparentImageDataUrl());
        img.setAttribute('alt', img.getAttribute('alt') || 'Remote image blocked');
        img.setAttribute('title', 'Remote images blocked');
        remoteBlocked += 1;
      }
      return;
    }
    if (isBlockedEmailUrl(trimmed)) img.removeAttribute('src');
  });

  collapseQuotedReplies(doc);

  const safeBody = doc.body ? doc.body.innerHTML : '';
  return {
    html: `<!doctype html><html><head><meta charset="utf-8"><style>html,body{margin:0;padding:0;background:white;color:#0a0a0a;font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;overflow-wrap:anywhere;}img{max-width:100%;height:auto;}blockquote{border-left:3px solid #d9d7d1;margin:1em 0;padding-left:1em;color:#525252;}table{max-width:100%;}pre{white-space:pre-wrap;}details.envelope-quote{margin:1em 0;}summary.envelope-quote-toggle{cursor:pointer;color:#2563eb;font-size:13px;list-style:none;user-select:none;padding:2px 0;outline:none;}summary.envelope-quote-toggle::-webkit-details-marker{display:none;}summary.envelope-quote-toggle::marker{content:'';}</style></head><body>${safeBody}</body></html>`,
    remoteBlocked,
  };
}

function renderRemoteImageControl(body, blockedCount) {
  if (!blockedCount || state.loadRemoteImages) return;
  const notice = el('div', { class: 'remote-image-notice' });
  notice.appendChild(el('span', { text: `Remote images blocked (${blockedCount}).` }));
  const button = el('button', { class: 'btn-ghost', text: 'Load remote images' });
  button.onclick = () => {
    state.loadRemoteImages = true;
    renderReader();
  };
  notice.appendChild(button);
  body.appendChild(notice);
}

function renderReader() {
  const msg = state.currentMessage;
  if (!msg) return;
  $('reader-meta').classList.remove('hidden');
  $('reader-subject').textContent = msg.subject || '(no subject)';
  $('reader-from').textContent = msg.from_addr || '';
  $('reader-to').textContent = msg.to_addr || '';
  $('reader-date').textContent = msg.date || '';
  $('reader-account-row').classList.remove('hidden');
  $('reader-account').textContent = `${messageAccountLabel(msg)} · ${messageFolder(msg)}`;
  const unread = isMessageUnread(msg);
  $('reader-read-state').textContent = unread ? 'Unread — opening does not mark read' : 'Read';
  $('btn-reader-mark-read').textContent = unread ? 'Mark Read' : 'Mark Unread';
  const t = threadContext(msg);
  if (t) {
    $('reader-thread-row').classList.remove('hidden');
    $('reader-thread').textContent = threadMetaText(msg) || t.thread_id;
    const url = threadContextUrl(msg);
    if (url) $('reader-thread-link').href = url;
  } else {
    $('reader-thread-row').classList.add('hidden');
    $('reader-thread').textContent = '';
    $('reader-thread-link').removeAttribute('href');
  }
  if (msg.cc_addr) {
    $('reader-cc-row').classList.remove('hidden');
    $('reader-cc').textContent = msg.cc_addr;
  } else {
    $('reader-cc-row').classList.add('hidden');
  }

  // Render body — HTML goes in a sandboxed iframe, text in a <pre>
  const body = $('reader-body');
  clear(body);
  if (msg.html_body) {
    const rendered = sanitizeEmailHtml(msg.html_body, msg, state.loadRemoteImages);
    renderRemoteImageControl(body, rendered.remoteBlocked);
    const frame = document.createElement('iframe');
    // Same-origin is granted for height measurement only; scripts/forms/top-nav
    // stay blocked (see file header). Email-controlled code never runs.
    frame.setAttribute('sandbox', 'allow-same-origin');
    frame.className = 'email-frame';
    frame.srcdoc = rendered.html;
    frame.addEventListener('load', () => {
      sizeReaderFrameToContent(frame);
      attachQuoteToggleRemeasure(frame);
    });
    body.appendChild(frame);
  } else if (msg.text_body) {
    body.appendChild(el('pre', { text: msg.text_body }));
  } else {
    body.appendChild(el('p', { class: 'text-mid text-sm', text: '(empty)' }));
  }

  // Attachments
  const attachRow = $('reader-attachments');
  clear(attachRow);
  if (msg.attachments && msg.attachments.length > 0) {
    const inlineAttachments = msg.attachments.filter(isInlineAttachment);
    const downloadableAttachments = msg.attachments.filter(a => !isInlineAttachment(a));
    attachRow.classList.remove('hidden');
    if (inlineAttachments.length) {
      attachRow.appendChild(el('p', { class: 'section-label mb-2', text: 'Inline images' }));
      for (const a of inlineAttachments) {
        const label = el('div', { class: 'attachment-inline text-xs font-mono text-mid' });
        label.textContent = `${a.filename} (${formatSize(a.size)}) · embedded in message`;
        attachRow.appendChild(label);
      }
    }
    if (downloadableAttachments.length) {
      attachRow.appendChild(el('p', { class: 'section-label mb-2 mt-3', text: 'Downloadable attachments' }));
      for (const a of downloadableAttachments) {
        const link = document.createElement('a');
        link.href = attachmentUrl(msg, a, false);
        link.className = 'block text-xs font-mono text-accent hover:underline';
        link.setAttribute('download', '');
        link.textContent = `${a.filename} (${formatSize(a.size)})`;
        attachRow.appendChild(link);
      }
    }
  } else {
    attachRow.classList.add('hidden');
  }

  $('reader').classList.add('show');
}

// Shrink/grow the HTML email iframe to fit its rendered content (Gmail-style),
// so short messages don't sit inside a tall empty box. Clamped to a sane max
// so enormous newsletters still scroll within the reader rather than the page.
function sizeReaderFrameToContent(frame) {
  try {
    const doc = frame.contentDocument || frame.contentWindow?.document;
    if (!doc || !doc.documentElement) return;
    // Neutralize the CSS min-height/height first and collapse the frame, so
    // scrollHeight reflects the TRUE content height rather than the forced
    // viewport height the CSS would otherwise report back.
    frame.style.minHeight = '0';
    frame.style.height = '0px';
    void frame.offsetHeight; // force reflow before measuring
    const contentHeight = Math.max(
      doc.documentElement.scrollHeight,
      doc.body ? doc.body.scrollHeight : 0,
    );
    if (!contentHeight) {
      frame.style.height = '';
      frame.style.minHeight = '';
      return;
    }
    const clamped = Math.min(Math.max(contentHeight + 24, 120), 760);
    frame.style.height = `${clamped}px`;
  } catch (_) {
    // Cross-origin or detached frame — restore the CSS default height.
    frame.style.height = '';
    frame.style.minHeight = '';
  }
}

// Re-measure the iframe whenever a collapsed quote is expanded/collapsed. The
// <details> toggle is native (no email script runs); the parent listens via
// the granted same-origin access and updates the summary label + frame height.
function attachQuoteToggleRemeasure(frame) {
  try {
    const doc = frame.contentDocument || frame.contentWindow?.document;
    if (!doc) return;
    doc.querySelectorAll('details.envelope-quote').forEach(details => {
      details.addEventListener('toggle', () => {
        const summary = details.querySelector('summary.envelope-quote-toggle');
        if (summary) summary.textContent = details.open ? 'Hide quoted text' : 'Show quoted text';
        sizeReaderFrameToContent(frame);
      });
    });
  } catch (_) {
    // Cross-origin or detached frame — nothing to wire.
  }
}

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function closeReader() {
  state.currentMessage = null;
  $('reader').classList.remove('show');
  renderReaderEmpty();
}

// ── Snoozed view ───────────────────────────────────────────────────
async function loadSnoozed() {
  if (!state.currentAccount) return;
  $('list-title').textContent = '★ Snoozed';
  try {
    const data = await api('GET', `/accounts/${state.currentAccount.id}/snoozed`);
    state.snoozed = data.snoozed || [];
    renderSnoozed();
    $('list-count').textContent = `${state.snoozed.length} snoozed`;
  } catch (e) { toast('Snoozed: ' + e.message, 'error'); }
}

function renderSnoozed() {
  const list = $('message-list');
  clear(list);
  state.renderedMessageKeys = [];
  pruneSelectedMessagesToRendered();
  updateBulkToolbar();
  if (state.snoozed.length === 0) {
    list.appendChild(el('div', { class: 'px-4 py-12 text-center text-sm text-mid', text: 'No snoozed messages.' }));
    return;
  }
  const now = new Date();
  for (const s of state.snoozed) {
    const row = el('div', { class: 'msg-row' });
    const ret = new Date(s.return_at);
    const overdue = ret < now;
    const label = (s.reason || 'no reason') + (s.note ? ' · ' + s.note : '');
    row.appendChild(el('div', { class: 'from', text: label }));
    row.appendChild(el('div', { class: 'subject', text: s.subject || '(no subject)' }));
    const dateCell = el('div', { class: 'date', text: (overdue ? 'overdue: ' : 'returns: ') + s.return_at });
    if (overdue) dateCell.style.color = '#c4421a';
    row.appendChild(dateCell);
    row.onclick = async () => {
      if (!confirm(`Unsnooze and return to ${s.original_folder}?`)) return;
      try {
        await api('POST', `/accounts/${state.currentAccount.id}/snoozed/${s.id}/unsnooze`);
        toast('Unsnoozed', 'success');
        loadSnoozed();
        loadFolders();
      } catch (e) { toast('Unsnooze failed: ' + e.message, 'error'); }
    };
    list.appendChild(row);
  }
}

// ── Agent Cockpit drafts ───────────────────────────────────────────
async function loadDrafts() {
  if (!state.currentAccount) {
    state.drafts = [];
    state.cockpitStatus = 'select an account';
    renderDrafts();
    return;
  }

  state.cockpitStatus = 'loading drafts...';
  renderDrafts();
  try {
    const data = await api('GET', `/accounts/${state.currentAccount.id}/drafts`);
    state.drafts = data.drafts || [];
    state.cockpitStatus = `${state.drafts.length} draft${state.drafts.length === 1 ? '' : 's'}`;
    renderDrafts();
  } catch (e) {
    state.drafts = [];
    state.cockpitStatus = 'drafts unavailable';
    state.cockpitMessage = { kind: 'error', text: 'Drafts: ' + e.message };
    renderDrafts();
  }
}

async function openDraftDeepLink(draftId) {
  try {
    const data = await api(
      'GET',
      `/accounts/${state.currentAccount.id}/drafts/${encodeURIComponent(draftId)}`
    );
    state.currentDraft = data.draft;
    upsertDraft(data.draft);
    renderDrafts();
    openComposerFromDraft(data.draft);
  } catch (e) {
    state.currentDraft = null;
    state.cockpitStatus = 'draft not found';
    state.cockpitMessage = {
      kind: 'error',
      text: `Draft "${draftId}" was not found for ${state.currentAccount.username}.`,
    };
    renderDrafts();
    toast('Draft deep link not found', 'error');
  }
}

async function openDraftFromMessageLink(accountId, uid, folder, msg) {
  // The message route briefly opens the reader while fetching; Drafts links
  // should land in the editor, not leave a read-only message panel behind it.
  $('reader').classList.remove('show');
  try {
    const data = await api(
      'GET',
      `/accounts/${accountId}/drafts/by-imap-uid/${encodeURIComponent(uid)}`
    );
    state.currentDraft = data.draft;
    upsertDraft(data.draft);
    renderDrafts();
    openComposerFromDraft(data.draft);
    return;
  } catch (_) {
    let draftMessage = msg;
    if (!draftMessage) {
      try {
        const messageData = await api(
          'GET',
          `/accounts/${accountId}/messages/${uid}?folder=${encodeURIComponent(folder)}`
        );
        const account = accountById(accountId);
        draftMessage = Object.assign({}, messageData.message, {
          account_id: accountId,
          account_username: account?.username || accountId || '',
          account_display_name: account?.display_name || null,
          folder,
        });
      } catch (messageError) {
        $('reader-subject').textContent = 'Error';
        clear($('reader-body'));
        $('reader-body').appendChild(el('div', { class: 'text-center text-warn py-12', text: messageError.message }));
        $('reader').classList.add('show');
        return;
      }
    }
    // Some Drafts-folder messages are server-only IMAP drafts with no local
    // Envelope draft record. There is no revision-safe way to edit them here,
    // so refuse with a review path rather than compose-copy-and-delete.
    refuseRawImapDraftEdit(draftMessage);
  }
}

function upsertDraft(draft) {
  if (!draft || !draft.id) return;
  const idx = state.drafts.findIndex(d => d.id === draft.id);
  if (idx >= 0) {
    state.drafts[idx] = draft;
  } else {
    state.drafts.unshift(draft);
  }
}

function renderDrafts() {
  const status = $('cockpit-status');
  const message = $('cockpit-message');
  const list = $('cockpit-drafts');
  if (!list) return;

  if (status) status.textContent = state.cockpitStatus || '';
  if (message) {
    clear(message);
    if (state.cockpitMessage && state.cockpitMessage.text) {
      message.className = `cockpit-message ${state.cockpitMessage.kind || ''}`;
      message.textContent = state.cockpitMessage.text;
      message.classList.remove('hidden');
    } else {
      message.className = 'cockpit-message hidden';
    }
  }

  clear(list);
  if (!state.currentAccount) {
    list.appendChild(el('div', { class: 'draft-empty', text: 'Select an account to see local drafts.' }));
    return;
  }

  if (state.currentDraft) {
    list.appendChild(renderDraftDetail(state.currentDraft, true));
  }

  const remaining = state.drafts.filter(d => !state.currentDraft || d.id !== state.currentDraft.id);
  if (remaining.length === 0 && !state.currentDraft) {
    list.appendChild(el('div', { class: 'draft-empty', text: 'No local drafts for this account.' }));
    return;
  }

  for (const draft of remaining) {
    list.appendChild(renderDraftSummary(draft));
  }
}

function renderDraftSummary(draft) {
  const row = el('div', { class: 'draft-row' });
  const path = dashboardPathForDraft(draft);
  const link = document.createElement('a');
  link.href = path;
  link.className = 'draft-row-subject';
  link.textContent = draft.subject || '(no subject)';
  link.title = draft.id || '';
  link.onclick = (e) => {
    e.preventDefault();
    history.pushState({}, '', path);
    state.route = parseDashboardRoute(window.location.pathname);
    openDraftDeepLink(draft.id);
  };

  row.appendChild(link);
  row.appendChild(el('span', { class: 'draft-row-to', text: draft.to_addr || '(no recipient)' }));
  row.appendChild(el('span', { class: 'draft-row-date', text: formatDate(draft.updated_at) }));
  return row;
}

function renderDraftDetail(draft, highlighted = false) {
  const card = el('article', {
    class: `draft-card ${highlighted ? 'highlight' : ''}`,
    data: { draftId: draft.id || '' },
  });

  const header = el('div', { class: 'draft-card-header' });
  header.appendChild(el('div', { class: 'draft-card-subject', text: draft.subject || '(no subject)' }));
  header.appendChild(el('div', { class: 'draft-card-status', text: draft.status || 'draft' }));
  card.appendChild(header);

  const meta = el('div', { class: 'draft-meta' });
  addDraftMeta(meta, 'To', draft.to_addr);
  addDraftMeta(meta, 'Cc', draft.cc_addr);
  addDraftMeta(meta, 'Bcc', draft.bcc_addr);
  addDraftMeta(meta, 'Reply-To', draft.reply_to);
  addDraftMeta(meta, 'Folder', draft.folder);
  addDraftMeta(meta, 'IMAP UID', draft.imap_uid);
  addDraftMeta(meta, 'Created by', draft.created_by);
  addDraftMeta(meta, 'Updated', draft.updated_at);
  addDraftMeta(meta, 'Send after', draft.send_after);
  card.appendChild(meta);

  const body = draft.text_content || draft.html_content || '(empty body)';
  card.appendChild(el('pre', { class: 'draft-body', text: body }));

  const actions = el('div', { class: 'draft-actions' });
  const edit = el('button', { class: 'btn-primary text-xs', text: 'Edit copy' });
  edit.onclick = () => openComposerFromDraft(draft);
  actions.appendChild(edit);

  const sendCli = el('button', { class: 'btn-ghost text-xs', text: 'Copy send CLI' });
  sendCli.onclick = () => copyText(currentDraftCli('send', draft));
  actions.appendChild(sendCli);

  const discardCli = el('button', { class: 'btn-ghost text-xs', text: 'Copy discard CLI' });
  discardCli.onclick = () => copyText(currentDraftCli('discard', draft));
  actions.appendChild(discardCli);

  const permalink = document.createElement('a');
  permalink.href = dashboardPathForDraft(draft);
  permalink.className = 'btn-ghost text-xs';
  permalink.textContent = 'Permalink';
  actions.appendChild(permalink);

  card.appendChild(actions);
  return card;
}

function addDraftMeta(parent, label, value) {
  if (value === undefined || value === null || value === '') return;
  const row = el('div', { class: 'draft-meta-row' });
  row.appendChild(el('span', { class: 'draft-meta-label', text: label + ':' }));
  row.appendChild(el('span', { class: 'draft-meta-value', text: String(value), title: String(value) }));
  parent.appendChild(row);
}

function currentDraftCli(action, draft) {
  const account = state.currentAccount ? (state.currentAccount.username || state.currentAccount.id) : draft.account_id;
  return `envelope draft ${action} ${draft.id} --account ${account}`;
}

// ── Composer ───────────────────────────────────────────────────────
function setBodyFormat(format) {
  state.bodyFormat = format === 'html' ? 'html' : 'text';
  const textButton = $('format-text');
  const htmlButton = $('format-html');
  textButton.classList.toggle('is-active', state.bodyFormat === 'text');
  htmlButton.classList.toggle('is-active', state.bodyFormat === 'html');
  textButton.setAttribute('aria-pressed', state.bodyFormat === 'text' ? 'true' : 'false');
  htmlButton.setAttribute('aria-pressed', state.bodyFormat === 'html' ? 'true' : 'false');
}

function composerAccount() {
  const accountId = $('composer-from')?.value || '';
  return state.accounts.find(account => account.id === accountId) || null;
}

function composerAccountLabel(account) {
  if (!account) return 'Choose an account before sending.';
  const name = account.display_name || account.name || '';
  const address = account.username || '';
  return name && address && name !== address
    ? `Sending from ${name} <${address}>`
    : `Sending from ${address || name}`;
}

function populateComposerAccounts(preferredAccountId = '') {
  const select = $('composer-from');
  clear(select);
  if (state.accounts.length === 0) {
    const option = document.createElement('option');
    option.value = '';
    option.textContent = 'No accounts available';
    select.appendChild(option);
    select.disabled = true;
    return;
  }

  select.disabled = false;
  for (const account of state.accounts) {
    const option = document.createElement('option');
    option.value = account.id;
    const name = account.display_name || account.name || account.username;
    option.textContent = name === account.username ? name : `${name} <${account.username}>`;
    select.appendChild(option);
  }
  const preferred = state.accounts.some(account => account.id === preferredAccountId)
    ? preferredAccountId
    : (state.currentAccount?.id || state.accounts[0].id);
  select.value = preferred;
}

function validComposerAddresses(raw) {
  const addresses = String(raw || '').split(',').map(value => value.trim()).filter(Boolean);
  return addresses.length > 0
    && addresses.every(address => {
      const angleAddress = address.match(/<([^>]+)>/);
      const normalized = (angleAddress?.[1] || address).trim();
      return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(normalized);
    });
}

function setComposerCheck(id, text, kind = '') {
  const check = $(id);
  check.textContent = text;
  check.classList.toggle('is-ready', kind === 'ready');
  check.classList.toggle('is-error', kind === 'error');
}

function updateComposerReview({ preserveStatus = false } = {}) {
  const to = $('composer-to').value.trim();
  const subject = $('composer-subject').value.trim();
  const account = composerAccount();
  const recipientReady = validComposerAddresses(to);
  const subjectReady = subject.length > 0;
  const deliveryReady = Boolean(account);

  setComposerCheck(
    'composer-recipient-check',
    recipientReady ? 'Ready' : (to ? 'Check address' : 'Required'),
    recipientReady ? 'ready' : (to ? 'error' : '')
  );
  setComposerCheck(
    'composer-subject-check',
    subjectReady ? 'Ready' : 'Required',
    subjectReady ? 'ready' : ''
  );
  setComposerCheck(
    'composer-delivery-check',
    deliveryReady ? 'Account ready' : 'Select account',
    deliveryReady ? 'ready' : 'error'
  );

  $('composer-account-context').textContent = composerAccountLabel(account);
  const canSend = recipientReady && subjectReady && deliveryReady && !state.composerSending;
  $('btn-composer-send').disabled = !canSend;

  if (!preserveStatus && !state.composerSending) {
    const status = $('composer-status');
    status.classList.remove('is-error');
    status.textContent = canSend ? 'Ready to send.' : 'Complete the required fields before sending.';
  }
  return canSend;
}

function renderComposerAttachments() {
  const list = $('attach-list');
  const empty = $('composer-attachments-empty');
  const count = $('composer-attachment-count');
  clear(list);

  const total = state.pendingAttachments.length;
  count.textContent = total === 0 ? 'None' : `${total} file${total === 1 ? '' : 's'}`;
  empty.hidden = total > 0;

  state.pendingAttachments.forEach((attachment, index) => {
    const item = el('li', { class: 'composer-attachment-item' });
    item.appendChild(el('span', {
      class: 'composer-attachment-name',
      text: attachment.filename,
      title: attachment.filename,
    }));
    item.appendChild(el('span', { text: formatSize(attachment.size || 0) }));
    const remove = el('button', {
      class: 'composer-attachment-remove',
      type: 'button',
      text: 'Remove',
      aria: { label: `Remove ${attachment.filename}` },
    });
    remove.onclick = () => {
      state.pendingAttachments.splice(index, 1);
      renderComposerAttachments();
    };
    item.appendChild(remove);
    list.appendChild(item);
  });
}

function openComposer(mode = 'new', parent = null) {
  // Reply/Reply-All require a parent message. Guard against being invoked
  // (e.g. via keyboard shortcut) with nothing selected — fall back to a
  // friendly toast instead of opening an empty "reply" with no recipient.
  if ((mode === 'reply' || mode === 'reply-all') && !parent) {
    toast('Open a message first to reply', '');
    return;
  }
  state.composeMode = mode;
  state.composeParent = parent;
  state.composeDraft = null;
  state.composerSending = false;
  state.pendingAttachments = [];

  const title = mode === 'reply' ? 'Reply' : mode === 'reply-all' ? 'Reply all' : 'New message';
  $('composer-title').textContent = title;
  $('composer-mode').textContent = mode === 'new' ? 'New' : mode.replace('-', ' ');

  const preferredAccountId = parent?.account_id || state.currentAccount?.id || '';
  populateComposerAccounts(preferredAccountId);

  if (mode === 'new') {
    $('composer-to').value = '';
    $('composer-cc').value = '';
    $('composer-subject').value = '';
    $('composer-body').value = '';
  } else if (parent) {
    $('composer-to').value = parent.from_addr || '';
    $('composer-cc').value = '';
    $('composer-subject').value = prefixRe(parent.subject || '');
    const quoted = (parent.text_body || '').split('\n').map(l => '> ' + l).join('\n');
    $('composer-body').value = '\n\n--- On ' + (parent.date || '?') + ', ' + (parent.from_addr || '?') + ' wrote: ---\n' + quoted;
  }

  setBodyFormat('text');
  renderComposerAttachments();
  updateComposerReview();
  $('composer-backdrop').classList.add('show');
  $('composer-backdrop').setAttribute('aria-hidden', 'false');
  $('composer').classList.add('show');
  $('composer').setAttribute('aria-hidden', 'false');
  requestAnimationFrame(() => {
    (mode === 'new' ? $('composer-to') : $('composer-body')).focus();
  });
}

function openComposerFromDraft(draft) {
  openComposer('new');
  // Editing an existing local draft edits and queues the SAME draft through
  // the revision-bound /edit + /send endpoints (expected_revision comes from
  // this viewed draft; the send uses the fresh revision returned by /edit).
  // The composer never composes a copy and never deletes mailbox messages:
  // provider Drafts cleanup is owned exclusively by the scheduled-send sweep,
  // after SMTP acceptance and durable sent state.
  state.composeMode = 'draft';
  state.composeDraft = {
    accountId: draft.account_id,
    draftId: draft.id,
    expectedRevision: typeof draft.revision === 'number' ? draft.revision : 0,
  };
  $('composer-to').value = draft.to_addr || '';
  $('composer-cc').value = draft.cc_addr || '';
  $('composer-subject').value = draft.subject || '';
  $('composer-body').value = draft.text_content || draft.html_content || '';
  setBodyFormat(draft.html_content && !draft.text_content ? 'html' : 'text');
  $('composer-title').textContent = 'Edit Draft';
  $('composer-mode').textContent = 'Draft';
  populateComposerAccounts(draft.account_id || source?.accountId || '');
  updateComposerReview();
}


// A Drafts-folder message with no corresponding local Envelope draft cannot be
// edited here safely: composing a copy and then deleting the original by
// guessed folder+UID risks removing the wrong message and double-sending.
// Fail closed with a clear review path instead — the CLI owns draft identity
// (it can import/modify the server-side draft with exact provenance).
function refuseRawImapDraftEdit(msg) {
  const uidLabel = msg && msg.uid != null ? ` (UID ${msg.uid})` : '';
  const account = msg?.account_username || msg?.account_id || '<account>';
  $('reader').classList.remove('show');
  toast(
    `This server-side draft${uidLabel} has no local Envelope draft record, so it cannot be edited safely from the dashboard. ` +
    `Review it with the CLI instead: envelope draft list --account ${account}`,
    'error'
  );
}

function prefixRe(subject) {
  return /^re:\s/i.test(subject) ? subject : 'Re: ' + subject;
}

function closeComposer() {
  $('composer-backdrop').classList.remove('show');
  $('composer-backdrop').setAttribute('aria-hidden', 'true');
  $('composer').classList.remove('show');
  $('composer').setAttribute('aria-hidden', 'true');
  state.composeMode = 'new';
  state.composeParent = null;
  state.composeDraft = null;
  state.composerSending = false;
  state.pendingAttachments = [];
  $('composer-attach').value = '';
}

async function sendComposer() {
  const sendAccountId = $('composer-from').value;
  if (!sendAccountId) {
    $('composer-status').textContent = 'Select a sending account.';
    $('composer-status').classList.add('is-error');
    return;
  }
  const to = $('composer-to').value.trim();
  const cc = $('composer-cc').value.trim();
  const subject = $('composer-subject').value.trim();
  const body = $('composer-body').value;
  const isHtml = state.bodyFormat === 'html';

  if (!updateComposerReview()) {
    $('composer-status').textContent = 'Check the recipient, subject, and sending account.';
    $('composer-status').classList.add('is-error');
    return;
  }

  state.composerSending = true;
  $('btn-composer-send').disabled = true;
  $('btn-composer-send').textContent = 'Queueing…';
  $('composer-status').classList.remove('is-error');
  $('composer-status').textContent = 'Queueing through Envelope…';

  try {
    if (state.composeMode === 'draft' && state.composeDraft) {
      // Edit and queue the SAME local draft, revision-bound end to end: the
      // edit carries the revision the human viewed; the send carries the new
      // revision returned by the edit. A 409 means the draft changed under
      // us — surface it, never overwrite or fall back to a copy.
      if (state.pendingAttachments.length) {
        throw new Error('Attachments cannot be added while editing an existing draft here; use `envelope draft modify --attach` instead.');
      }
      const d = state.composeDraft;
      const edited = await api('POST', `/accounts/${d.accountId}/drafts/${d.draftId}/edit`, {
        expected_revision: d.expectedRevision,
        to_addr: to,
        cc_addr: cc || null,
        subject,
        text_content: isHtml ? null : body,
        html_content: isHtml ? body : null,
      });
      await api('POST', `/accounts/${d.accountId}/drafts/${d.draftId}/send`, {
        confirm: true,
        expected_revision: edited.draft.revision,
      });
      toast('Draft queued for send', 'success');
    } else if (state.composeMode === 'reply' || state.composeMode === 'reply-all') {
      await api('POST', `/accounts/${sendAccountId}/compose/reply`, {
        parent_uid: state.composeParent.uid,
        parent_folder: state.composeParent?.folder || state.currentFolder,
        reply_all: state.composeMode === 'reply-all',
        text: isHtml ? null : body,
        html: isHtml ? body : null,
        attachments: state.pendingAttachments,
      });
      toast('Reply queued for send', 'success');
    } else {
      await api('POST', `/accounts/${sendAccountId}/compose`, {
        to,
        subject,
        text: isHtml ? null : body,
        html: isHtml ? body : null,
        cc: cc || null,
        attachments: state.pendingAttachments,
      });
      toast('Message queued for send', 'success');
    }
    closeComposer();
  } catch (e) {
    $('composer-status').textContent = 'Could not queue: ' + e.message;
    $('composer-status').classList.add('is-error');
    toast('Could not queue message: ' + e.message, 'error');
  } finally {
    state.composerSending = false;
    $('btn-composer-send').textContent = 'Send';
    if ($('composer').classList.contains('show')) updateComposerReview({ preserveStatus: true });
  }
}

async function handleAttachmentChange(e) {
  const files = Array.from(e.target.files || []);
  for (const f of files) {
    const buf = await f.arrayBuffer();
    const bytes = new Uint8Array(buf);
    let binary = '';
    const CHUNK = 0x8000;
    for (let i = 0; i < bytes.length; i += CHUNK) {
      binary += String.fromCharCode.apply(null, bytes.subarray(i, i + CHUNK));
    }
    state.pendingAttachments.push({
      filename: f.name,
      content_type: f.type || 'application/octet-stream',
      data_b64: btoa(binary),
      size: f.size,
    });
  }
  e.target.value = '';
  renderComposerAttachments();
}

// ── Add account modal ──────────────────────────────────────────────
function openAddAccount() {
  $('add-account-modal').classList.add('show');
  $('add-account-error').classList.add('hidden');
  $('discover-status').textContent = '';
}
function closeAddAccount() { $('add-account-modal').classList.remove('show'); }

async function runDiscover() {
  const email = $('new-email').value.trim();
  if (!email) { toast('Enter an email first', 'error'); return; }
  $('discover-status').textContent = 'Probing DNS…';
  try {
    const result = await api('POST', '/accounts/discover', { email });
    $('discover-status').textContent = `${result.imap_host}:${result.imap_port} / ${result.smtp_host}:${result.smtp_port}`;
  } catch (e) {
    $('discover-status').textContent = 'Discovery failed: ' + e.message;
  }
}

async function createAccount() {
  const email = $('new-email').value.trim();
  const password = $('new-password').value;
  const display_name = $('new-display').value.trim() || null;
  if (!email || !password) {
    const err = $('add-account-error');
    err.textContent = 'Email and password required';
    err.classList.remove('hidden');
    return;
  }
  try {
    const res = await api('POST', '/accounts', { email, password, display_name });
    toast('Account added', 'success');
    closeAddAccount();
    await loadAccounts();
    await loadStats();
    if (res.account) selectAccount(res.account);
  } catch (e) {
    const err = $('add-account-error');
    err.textContent = e.message;
    err.classList.remove('hidden');
  }
}

// ── Snooze modal ───────────────────────────────────────────────────
function openSnoozeModal() {
  if (!state.currentMessage) return;
  $('snooze-modal').classList.add('show');
  $('snooze-until').value = 'tomorrow';
}
function closeSnoozeModal() { $('snooze-modal').classList.remove('show'); }

// ── Delete / mark-read handlers ────────────────────────────────────
async function deleteCurrentMessage() {
  const accountId = messageAccountId();
  const folder = messageFolder();
  if (!state.currentMessage || !accountId) return;
  if (!confirm('Delete this message?')) return;
  try {
    await api(
      'DELETE',
      `/accounts/${accountId}/messages/${state.currentMessage.uid}?folder=${encodeURIComponent(folder)}`
    );
    toast('Deleted', 'success');
    closeReader();
    loadMessages();
    if (!isUnifiedView()) loadFolders();
  } catch (e) { toast('Delete failed: ' + e.message, 'error'); }
}

async function toggleMessageReadState(message) {
  const accountId = messageAccountId(message);
  const folder = messageFolder(message);
  if (!message || !accountId) return;
  const unread = isMessageUnread(message);
  try {
    await api(
      'POST',
      `/accounts/${accountId}/messages/${message.uid}/flags`,
      unread
        ? { folder, add: ['seen'], remove: [] }
        : { folder, add: [], remove: ['seen'] }
    );
    setMessageUnread(message, !unread);
    if (state.currentMessage && state.currentMessage.uid === message.uid && messageAccountId(state.currentMessage) === accountId) {
      setMessageUnread(state.currentMessage, !unread);
      renderReader();
    }
    toast(unread ? 'Marked read' : 'Marked unread', 'success');
    loadMessages();
    if (!isUnifiedView()) loadFolders();
  } catch (e) { toast('Flag failed: ' + e.message, 'error'); }
}

async function markCurrentRead() {
  if (!state.currentMessage) return;
  await toggleMessageReadState(state.currentMessage);
}

// ── Search ─────────────────────────────────────────────────────────
async function runSearch() {
  if (isUnifiedView()) {
    toast('Select an account to search a mailbox', 'error');
    return;
  }
  if (!state.currentAccount) return;
  const q = $('search-input').value.trim();
  if (!q) {
    clearSearch();
    return;
  }
  setSearchState(q);
  setRefresh('searching…');
  try {
    const data = await api(
      'GET',
      `/accounts/${state.currentAccount.id}/search?q=${encodeURIComponent(q)}&folder=${encodeURIComponent(state.currentFolder)}&limit=100`
    );
    state.messages = data.messages || [];
    renderMessages();
    const meta = folderMeta(state.currentFolder);
    const scope = meta ? ` in ${folderCountText(meta)}` : '';
    $('list-count').textContent = `${state.messages.length} search result${state.messages.length === 1 ? '' : 's'}${scope}`;
    setRefresh('ok');
  } catch (e) {
    setRefresh('error');
    toast('Search: ' + e.message, 'error');
  }
}

function clearSearch() {
  $('search-input').value = '';
  setSearchState('');
  loadMessages();
}

// ── Event wiring ───────────────────────────────────────────────────
function wireEvents() {
  wireBulkToolbar();
  $('account-switcher').onchange = (e) => {
    if (e.target.value === '__unified__') {
      selectUnifiedInbox();
      return;
    }
    const acct = state.accounts.find(a => a.id === e.target.value);
    if (acct) selectAccount(acct);
  };
  $('btn-refresh-folders').onclick = async () => {
    if (isUnifiedView()) {
      await selectUnifiedInbox({ refresh: true });
      return;
    }
    await loadFolders();
    await loadMessages();
  };
  $('btn-refresh-cockpit').onclick = loadCockpit;
  $('btn-toggle-cockpit').onclick = () => setCockpitExpanded(!state.cockpitExpanded);
  $('btn-refresh-rules').onclick = loadRules;
  $('btn-run-rules').onclick = runEnabledRulesForCurrentFolder;
  $('btn-add-account').onclick = openAddAccount;
  $('btn-add-account-close').onclick = closeAddAccount;
  $('btn-discover').onclick = runDiscover;
  $('btn-create-account').onclick = createAccount;

  $('btn-compose').onclick = () => openComposer('new');
  $('btn-composer-close').onclick = closeComposer;
  $('btn-composer-send').onclick = sendComposer;
  $('composer-attach').onchange = handleAttachmentChange;
  $('composer-from').onchange = () => updateComposerReview();
  $('composer-to').oninput = () => updateComposerReview();
  $('composer-subject').oninput = () => updateComposerReview();
  $('composer-body').onkeydown = (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') {
      event.preventDefault();
      sendComposer();
    }
  };
  $('composer-backdrop').onclick = (event) => {
    if (event.target === $('composer-backdrop') && !state.composerSending) closeComposer();
  };

  $('format-text').onclick = () => setBodyFormat('text');
  $('format-html').onclick = () => setBodyFormat('html');

  $('btn-reader-close').onclick = closeReader;
  $('btn-reader-reply').onclick = () => openComposer('reply', state.currentMessage);
  $('btn-reader-reply-all').onclick = () => openComposer('reply-all', state.currentMessage);
  $('btn-reader-test-rules').onclick = testRulesForCurrentMessage;
  $('btn-reader-delete').onclick = deleteCurrentMessage;
  $('btn-reader-mark-read').onclick = markCurrentRead;
  $('btn-reader-snooze').onclick = openSnoozeModal;

  $('btn-snooze-cancel').onclick = closeSnoozeModal;
  $('btn-snooze-confirm').onclick = () => {
    toast('Snooze from CLI for now: envelope snooze set <uid> --until ...', 'error');
    closeSnoozeModal();
  };

  $('btn-search').onclick = runSearch;
  $('btn-search-clear').onclick = clearSearch;
  $('search-input').onkeydown = (e) => { if (e.key === 'Enter') runSearch(); };

  // Gmail-style keyboard shortcuts + discoverable cheat sheet.
  document.addEventListener('keydown', handleGlobalKeydown);
  const sheetClose = $('btn-shortcut-sheet-close');
  if (sheetClose) sheetClose.onclick = () => toggleShortcutSheet(false);
  const sheet = $('shortcut-sheet');
  if (sheet) sheet.addEventListener('click', (e) => { if (e.target === sheet) toggleShortcutSheet(false); });
  const hint = $('btn-shortcut-hint');
  if (hint) hint.onclick = () => toggleShortcutSheet(true);

  // Restart the auto-refresh timer when the tab becomes visible again after
  // being hidden, so polling resumes without waiting a full extra interval.
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible' && autoRefreshTimer === null) {
      const delayMs = isUnifiedView() ? 90_000 : 120_000;
      scheduleAutoRefresh(delayMs);
    }
  });
}

// ── Dashboard deep links ───────────────────────────────────────────
function parseDashboardRoute(pathname = window.location.pathname, search = window.location.search) {
  const params = new URLSearchParams(search || '');
  const decode = (value) => {
    try { return decodeURIComponent(value); }
    catch (_) { return value; }
  };

  let match = pathname.match(/^\/accounts\/([^/]+)\/messages\/(\d+)$/);
  if (match) {
    return {
      kind: 'message',
      accountId: decode(match[1]),
      uid: Number(match[2]),
      folder: params.get('folder') || 'INBOX',
    };
  }

  match = pathname.match(/^\/accounts\/([^/]+)\/cockpit$/);
  if (match) return { kind: 'cockpit', accountId: decode(match[1]) };

  match = pathname.match(/^\/accounts\/([^/]+)\/rules$/);
  if (match) return { kind: 'rules', accountId: decode(match[1]) };

  match = pathname.match(/^\/accounts\/([^/]+)\/drafts\/([^/]+)$/);
  if (match) return { kind: 'draft', accountId: decode(match[1]), draftId: decode(match[2]) };

  return null;
}

async function applyDashboardRoute(route = parseDashboardRoute()) {
  if (!route) return false;
  const account = accountById(route.accountId);
  if (!account) {
    toast(`Dashboard link account not found: ${route.accountId}`, 'error');
    return false;
  }

  if (route.kind === 'message') {
    state.currentView = 'account';
    state.currentSmartMailbox = null;
    state.currentAccount = account;
    state.currentFolder = route.folder || 'INBOX';
    renderAccountSwitcher();
    renderAccountsList();
    renderSmartMailboxes();
    await openMessage(route.uid, route.accountId, route.folder || 'INBOX');
    return true;
  }

  await selectAccount(account, 'INBOX', {
    loadFolderList: false,
    loadMessageList: false,
    loadCockpitPanel: route.kind !== 'draft',
  });
  if (route.kind === 'rules') {
    $('rules-summary').scrollIntoView({ block: 'start' });
  } else if (route.kind === 'draft') {
    // Draft links are editor links. Behave like Gmail: fetch the draft and open
    // the composer directly, without steering the operator into Cockpit.
    await openDraftDeepLink(route.draftId);
  } else if (route.kind === 'cockpit') {
    $('cockpit-account').scrollIntoView({ block: 'start' });
  }
  return true;
}

// ── Boot ───────────────────────────────────────────────────────────
document.addEventListener('DOMContentLoaded', async () => {
  await primeCsrf();
  state.route = parseDashboardRoute(window.location.pathname);
  wireEvents();
  setCockpitExpanded(false);
  renderReaderEmpty();
  renderDrafts();
  renderSmartMailboxes();
  const route = parseDashboardRoute();
  if (route) {
    await loadAccounts({ autoSelect: false });
    const routed = await applyDashboardRoute(route);
    loadStats();
    setRefresh(routed ? 'linked' : 'ready');
  } else {
    await loadStats();
    await loadAccounts();
    setRefresh('ready');
  }
});
