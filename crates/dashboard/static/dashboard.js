// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2
//
// Safe rendering notes:
// - HTML email bodies render inside a sandboxed iframe (no scripts, no
//   same-origin access). Never assigned to innerHTML on the dashboard DOM.
// - Every piece of user-supplied text (subjects, addresses, filenames,
//   body excerpts) goes through textContent, never innerHTML.
// - DOM trees are built with createElement/appendChild, not template strings.

// ── State ──────────────────────────────────────────────────────────
const state = {
  accounts: [],
  currentAccount: null,
  folders: [],
  currentFolder: 'INBOX',
  messages: [],
  currentMessage: null,
  drafts: [],
  currentDraft: null,
  snoozed: [],
  composeMode: 'new',
  composeParent: null,
  pendingAttachments: [],
  bodyFormat: 'text',
  showAllAccounts: false,
  searchQuery: '',
  rules: [],
  route: null,
  cockpitStatus: 'select an account',
  cockpitMessage: null,
};

// ── Dashboard routes ───────────────────────────────────────────────
function safeDecodeSegment(segment) {
  try { return decodeURIComponent(segment); }
  catch (_) { return segment; }
}

function parseDashboardRoute(pathname) {
  const parts = String(pathname || '/').split('/').filter(Boolean).map(safeDecodeSegment);
  if (parts.length === 0) return null;

  if (parts[0] === 'accounts' && parts.length >= 3) {
    if (parts[2] === 'drafts' && parts[3]) {
      return { kind: 'draft', accountSlug: parts[1], draftId: parts[3] };
    }
    if (parts[2] === 'cockpit') {
      return { kind: 'cockpit', accountSlug: parts[1] };
    }
  }

  if (parts[1] === 'drafts' && parts[2]) {
    return { kind: 'draft', accountSlug: parts[0], draftId: parts[2] };
  }
  if (parts[1] === 'cockpit') {
    return { kind: 'cockpit', accountSlug: parts[0] };
  }

  return null;
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
  if (!accountId || !draft.id) return '#';
  return `/accounts/${encodeURIComponent(accountId)}/drafts/${encodeURIComponent(draft.id)}`;
}

function focusAgentCockpit() {
  const panel = $('agent-cockpit');
  if (!panel) return;
  panel.scrollIntoView({ behavior: 'smooth', block: 'start' });
  panel.focus({ preventScroll: true });
}

// ── Fetch helper ───────────────────────────────────────────────────
async function api(method, path, body) {
  const opts = { method, headers: { 'Accept': 'application/json' } };
  if (body !== undefined) {
    opts.headers['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
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

function folderCountText(f) {
  if (!f) return 'unavailable';
  const unseen = f.unseen || 0;
  const exists = f.exists || 0;
  return unseen > 0 ? `${unseen} unread / ${exists} total` : `${exists} total`;
}

function updateCurrentFolderStats() {
  const inbox = findFolderByKind('inbox');
  const drafts = findFolderByKind('drafts');
  setStat('stat-unread', inbox ? (inbox.unseen || 0) : null);
  setStat('stat-drafts', drafts ? (drafts.exists || 0) : null);
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

// ── Rules ──────────────────────────────────────────────────────────
async function loadRules() {
  const summary = $('rules-summary');
  const list = $('rules-list');
  clear(list);
  if (!state.currentAccount) {
    summary.textContent = 'select an account';
    state.rules = [];
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
    const card = el('div', { class: `rule-card ${rule.enabled ? 'enabled' : 'disabled'}` });
    card.appendChild(el('div', { class: 'rule-title', text: rule.name || '(unnamed rule)', title: rule.id || '' }));
    card.appendChild(el('div', { class: 'rule-meta', text: `priority ${rule.priority} · ${rule.enabled ? 'enabled' : 'disabled'} · ${rule.stop ? 'stop' : 'continue'} · ${rule.hit_count || 0} hits` }));
    card.appendChild(el('div', { class: 'rule-action', text: ruleActionText(rule.action), title: rule.action || '' }));
    card.appendChild(el('div', { class: 'rule-match', text: ruleMatchText(rule.match_expr), title: rule.match_expr || '' }));
    list.appendChild(card);
  }
}

function currentRulesRunCli(limit) {
  if (!state.currentAccount) return '';
  const account = state.currentAccount.username || state.currentAccount.id;
  return `envelope rule run --account ${account} --folder ${state.currentFolder} --limit ${limit} --json`;
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
  if (!confirm(`Run ${enabled} enabled rule${enabled === 1 ? '' : 's'} against latest ${limit} message${limit === 1 ? '' : 's'} in ${folderLabel}?`)) return;

  $('btn-run-rules').disabled = true;
  $('rules-run-status').textContent = `running ${enabled} rule${enabled === 1 ? '' : 's'} on ${folderLabel}…`;
  const log = $('rules-run-log');
  clear(log);
  log.classList.add('hidden');

  try {
    const result = await api('POST', `/accounts/${state.currentAccount.id}/rules/run`, {
      folder: state.currentFolder,
      limit,
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
  if (!state.currentAccount || !state.currentMessage) return;
  const panel = $('reader-rules-panel');
  clear(panel);
  panel.classList.remove('hidden');
  panel.appendChild(el('div', { class: 'text-xs font-mono text-mid', text: 'Dry-running enabled rules…' }));
  try {
    const data = await api(
      'GET',
      `/accounts/${state.currentAccount.id}/rules/test/${state.currentMessage.uid}?folder=${encodeURIComponent(state.currentFolder)}`
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
    setStat('stat-accounts', stats.accounts ?? 0);
    setStat('stat-snoozed', stats.snoozed ?? 0);
  } catch (e) {
    setStat('stat-accounts', 'error');
    setStat('stat-snoozed', 'error');
    console.error('loadStats', e);
  }
}

// ── Accounts ───────────────────────────────────────────────────────
async function loadAccounts() {
  try {
    const data = await api('GET', '/accounts');
    state.accounts = data.accounts || [];
    renderAccountSwitcher();
    renderAccountsList();
    if (!state.currentAccount && state.accounts.length > 0) {
      const routeAccount = state.route ? resolveAccountSlug(state.route.accountSlug) : null;
      if (state.route && !routeAccount) {
        state.cockpitStatus = 'deep link account not found';
        state.cockpitMessage = {
          kind: 'error',
          text: `No dashboard account matched "${state.route.accountSlug}".`,
        };
        renderDrafts();
        focusAgentCockpit();
        toast('Deep link account not found', 'error');
        return;
      }
      await selectAccount(routeAccount || state.accounts[0]);
      await applyDashboardRoute();
    } else if (state.accounts.length === 0) {
      state.cockpitStatus = 'no accounts';
      state.cockpitMessage = { kind: 'error', text: 'No accounts are configured in this dashboard.' };
      renderDrafts();
    }
  } catch (e) {
    toast('Failed to load accounts: ' + e.message, 'error');
  }
}

function renderAccountSwitcher() {
  const sel = $('account-switcher');
  clear(sel);
  sel.appendChild(el('option', { text: 'Select account...' }));
  sel.firstChild.value = '';
  for (const acct of state.accounts) {
    const opt = el('option', { text: acct.username });
    opt.value = acct.id;
    if (state.currentAccount && state.currentAccount.id === acct.id) opt.selected = true;
    sel.appendChild(opt);
  }
}

function renderAccountsList() {
  const list = $('accounts-list');
  clear(list);
  const COLLAPSED_LIMIT = 3;
  const showAll = state.accounts.length <= COLLAPSED_LIMIT || state.showAllAccounts;
  const visible = showAll ? state.accounts : state.accounts.slice(0, COLLAPSED_LIMIT);

  for (const acct of visible) {
    const emailSpan = el('span', { class: 'email', text: acct.username, title: acct.username });
    const delBtn = el('button', { text: '×' });
    delBtn.onclick = async () => {
      if (!confirm(`Delete account ${acct.username}?`)) return;
      try {
        await api('DELETE', `/accounts/${acct.id}`);
        toast('Account deleted', 'success');
        await loadAccounts();
        await loadStats();
      } catch (e) { toast('Delete failed: ' + e.message, 'error'); }
    };
    list.appendChild(el('div', { class: 'account-item' }, [emailSpan, delBtn]));
  }

  if (!showAll && state.accounts.length > COLLAPSED_LIMIT) {
    const more = el('div', {
      class: 'account-item',
      text: `+ ${state.accounts.length - COLLAPSED_LIMIT} more`,
      style: { cursor: 'pointer', color: '#8a8780', justifyContent: 'center' },
    });
    more.onclick = () => { state.showAllAccounts = true; renderAccountsList(); };
    list.appendChild(more);
  } else if (state.accounts.length > COLLAPSED_LIMIT) {
    const less = el('div', {
      class: 'account-item',
      text: 'show less',
      style: { cursor: 'pointer', color: '#8a8780', justifyContent: 'center' },
    });
    less.onclick = () => { state.showAllAccounts = false; renderAccountsList(); };
    list.appendChild(less);
  }
}

async function selectAccount(acct) {
  state.currentAccount = acct;
  state.currentFolder = 'INBOX';
  state.currentDraft = null;
  state.cockpitMessage = null;
  setSearchState('');
  $('search-input').value = '';
  renderAccountSwitcher();
  await loadRules();
  // Sequential — folders first (creates the IMAP connection), then messages reuse it.
  await loadFolders();
  await loadMessages();
  await loadDrafts();
}

// ── Folders ────────────────────────────────────────────────────────
async function loadFolders() {
  if (!state.currentAccount) return;
  setRefresh('loading folders…');
  setStat('stat-unread', 'loading');
  setStat('stat-drafts', 'loading');
  // Show loading state immediately
  const list = $('folder-list');
  clear(list);
  list.appendChild(el('div', { class: 'px-3 py-6 text-xs text-mid font-mono text-center', text: 'Loading folders…' }));
  try {
    const data = await api('GET', `/accounts/${state.currentAccount.id}/folders`);
    state.folders = data.folders || [];
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
    clear(list);
    const retry = el('button', { class: 'btn-ghost text-xs mt-2', text: 'Retry' });
    retry.onclick = loadFolders;
    list.appendChild(el('div', { class: 'px-3 py-4 text-xs text-warn font-mono', text: 'Failed to load folders' }));
    list.appendChild(retry);
    setRefresh('error');
    setStat('stat-unread', 'error');
    setStat('stat-drafts', 'error');
    toast('Folders: ' + e.message, 'error');
  }
}

function renderFolders(data) {
  const list = $('folder-list');
  clear(list);
  const sorted = [...(data.folders || [])].sort((a, b) => {
    if (a.folder === 'INBOX') return -1;
    if (b.folder === 'INBOX') return 1;
    return a.folder.localeCompare(b.folder);
  });

  for (const f of sorted) {
    const item = el('div', { class: 'folder-item' });
    if (f.folder === state.currentFolder) item.classList.add('active');
    if (f.unseen && f.unseen > 0) item.classList.add('has-unseen');
    const unseenLabel = f.unseen && f.unseen > 0 ? `${f.unseen} unread / ${f.exists}` : `${f.exists}`;
    item.appendChild(el('span', { class: 'name', text: displayFolderName(f.folder), title: f.folder }));
    item.appendChild(el('span', { class: 'count', text: unseenLabel }));
    item.onclick = () => {
      state.currentFolder = f.folder;
      setSearchState('');
      $('search-input').value = '';
      loadMessages();
      renderFolders(data);
    };
    list.appendChild(item);
  }

  if (data.snoozed_virtual) {
    const item = el('div', { class: 'folder-item' });
    if (state.currentFolder === '__snoozed__') item.classList.add('active');
    if (data.snoozed_virtual.exists > 0) item.classList.add('has-unseen');
    item.appendChild(el('span', { class: 'name', text: '★ Snoozed' }));
    item.appendChild(el('span', { class: 'count', text: String(data.snoozed_virtual.exists) }));
    item.onclick = () => {
      state.currentFolder = '__snoozed__';
      loadSnoozed();
      renderFolders(data);
    };
    list.appendChild(item);
  }
}

// ── Messages list ──────────────────────────────────────────────────
async function loadMessages() {
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

function renderMessages() {
  const list = $('message-list');
  clear(list);
  if (state.messages.length === 0) {
    list.appendChild(el('div', { class: 'px-4 py-12 text-center text-sm text-mid', text: 'No messages.' }));
    return;
  }
  const sorted = [...state.messages].sort((a, b) => (b.uid || 0) - (a.uid || 0));
  for (const m of sorted) {
    const row = el('div', { class: 'msg-row' });
    if (!(m.flags || []).some(f => f.toLowerCase().includes('seen'))) {
      row.classList.add('unseen');
    }
    row.appendChild(el('div', {
      class: 'from',
      text: m.from_addr || '(unknown)',
      title: m.from_addr || '',
    }));
    row.appendChild(el('div', {
      class: 'subject',
      text: m.subject || '(no subject)',
      title: m.subject || '',
    }));
    row.appendChild(el('div', { class: 'date', text: formatDate(m.date) }));
    row.onclick = () => openMessage(m.uid);
    list.appendChild(row);
  }
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
async function openMessage(uid) {
  if (!state.currentAccount) return;
  // Show reader immediately with loading state
  $('reader-subject').textContent = 'Loading…';
  $('reader-from').textContent = '';
  $('reader-to').textContent = '';
  $('reader-date').textContent = '';
  clear($('reader-body'));
  clearRuleTestPanel();
  $('reader-body').appendChild(el('div', { class: 'text-center text-mid py-12', text: 'Loading message…' }));
  $('reader').classList.add('show');
  try {
    const data = await api(
      'GET',
      `/accounts/${state.currentAccount.id}/messages/${uid}?folder=${encodeURIComponent(state.currentFolder)}`
    );
    state.currentMessage = data.message;
    renderReader();
  } catch (e) {
    $('reader-subject').textContent = 'Error';
    clear($('reader-body'));
    $('reader-body').appendChild(el('div', { class: 'text-center text-warn py-12', text: e.message }));
  }
}

function renderReader() {
  const msg = state.currentMessage;
  if (!msg) return;
  $('reader-subject').textContent = msg.subject || '(no subject)';
  $('reader-from').textContent = msg.from_addr || '';
  $('reader-to').textContent = msg.to_addr || '';
  $('reader-date').textContent = msg.date || '';
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
    const frame = document.createElement('iframe');
    frame.setAttribute('sandbox', ''); // strict sandbox: no scripts, no forms, no same-origin
    frame.style.width = '100%';
    frame.style.minHeight = '400px';
    frame.style.border = '0';
    frame.srcdoc = msg.html_body;
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
    attachRow.classList.remove('hidden');
    attachRow.appendChild(el('p', { class: 'section-label mb-2', text: 'Attachments' }));
    for (const a of msg.attachments) {
      const link = document.createElement('a');
      link.href = `/api/accounts/${state.currentAccount.id}/messages/${msg.uid}/attachments/${encodeURIComponent(a.filename)}?folder=${encodeURIComponent(state.currentFolder)}`;
      link.className = 'block text-xs font-mono text-accent hover:underline';
      link.setAttribute('download', '');
      link.textContent = `${a.filename} (${formatSize(a.size)})`;
      attachRow.appendChild(link);
    }
  } else {
    attachRow.classList.add('hidden');
  }

  $('reader').classList.add('show');
}

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function closeReader() {
  $('reader').classList.remove('show');
  state.currentMessage = null;
  clearRuleTestPanel();
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
async function applyDashboardRoute() {
  const route = state.route;
  if (!route || !state.currentAccount) return;

  if (route.kind === 'draft') {
    await openDraftDeepLink(route.draftId);
  } else if (route.kind === 'cockpit') {
    focusAgentCockpit();
  }
}

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
  state.cockpitStatus = 'loading draft...';
  state.cockpitMessage = null;
  renderDrafts();
  try {
    const data = await api(
      'GET',
      `/accounts/${state.currentAccount.id}/drafts/${encodeURIComponent(draftId)}`
    );
    state.currentDraft = data.draft;
    upsertDraft(data.draft);
    state.cockpitStatus = 'draft opened';
    renderDrafts();
    focusAgentCockpit();
  } catch (e) {
    state.currentDraft = null;
    state.cockpitStatus = 'draft not found';
    state.cockpitMessage = {
      kind: 'error',
      text: `Draft "${draftId}" was not found for ${state.currentAccount.username}.`,
    };
    renderDrafts();
    focusAgentCockpit();
    toast('Draft deep link not found', 'error');
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
  if (!status || !message || !list) return;

  status.textContent = state.cockpitStatus || '';
  clear(message);
  if (state.cockpitMessage && state.cockpitMessage.text) {
    message.className = `cockpit-message ${state.cockpitMessage.kind || ''}`;
    message.textContent = state.cockpitMessage.text;
    message.classList.remove('hidden');
  } else {
    message.className = 'cockpit-message hidden';
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
  $('format-text').className = state.bodyFormat === 'text'
    ? 'px-2 py-0.5 text-xs font-mono border border-ink bg-ink text-paper'
    : 'px-2 py-0.5 text-xs font-mono border border-rule text-mid';
  $('format-html').className = state.bodyFormat === 'html'
    ? 'px-2 py-0.5 text-xs font-mono border border-ink bg-ink text-paper'
    : 'px-2 py-0.5 text-xs font-mono border border-rule text-mid';
}

function openComposer(mode = 'new', parent = null) {
  state.composeMode = mode;
  state.composeParent = parent;
  state.pendingAttachments = [];

  const title = mode === 'reply' ? 'Reply' : mode === 'reply-all' ? 'Reply All' : 'New Message';
  $('composer-title').textContent = title;

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

  clear($('attach-list'));
  $('composer-status').textContent = '';
  $('composer').classList.add('show');
}

function openComposerFromDraft(draft) {
  openComposer('new');
  $('composer-to').value = draft.to_addr || '';
  $('composer-cc').value = draft.cc_addr || '';
  $('composer-subject').value = draft.subject || '';
  $('composer-body').value = draft.text_content || draft.html_content || '';
  setBodyFormat(draft.html_content && !draft.text_content ? 'html' : 'text');
  toast('Draft loaded into composer', 'success');
}


function prefixRe(subject) {
  return /^re:\s/i.test(subject) ? subject : 'Re: ' + subject;
}

function closeComposer() {
  $('composer').classList.remove('show');
  state.composeMode = 'new';
  state.composeParent = null;
  state.pendingAttachments = [];
}

async function sendComposer() {
  if (!state.currentAccount) {
    toast('No account selected', 'error');
    return;
  }
  const to = $('composer-to').value.trim();
  const cc = $('composer-cc').value.trim();
  const subject = $('composer-subject').value.trim();
  const body = $('composer-body').value;
  const isHtml = state.bodyFormat === 'html';

  if (!to || !subject) {
    toast('Recipient and subject required', 'error');
    return;
  }

  $('composer-status').textContent = 'Sending…';

  try {
    if (state.composeMode === 'reply' || state.composeMode === 'reply-all') {
      await api('POST', `/accounts/${state.currentAccount.id}/compose/reply`, {
        parent_uid: state.composeParent.uid,
        parent_folder: state.currentFolder,
        reply_all: state.composeMode === 'reply-all',
        text: isHtml ? null : body,
        html: isHtml ? body : null,
        attachments: state.pendingAttachments,
      });
      toast('Reply sent', 'success');
    } else {
      await api('POST', `/accounts/${state.currentAccount.id}/compose`, {
        to,
        subject,
        text: isHtml ? null : body,
        html: isHtml ? body : null,
        cc: cc || null,
        attachments: state.pendingAttachments,
      });
      toast('Sent', 'success');
    }
    closeComposer();
  } catch (e) {
    $('composer-status').textContent = '';
    toast('Send failed: ' + e.message, 'error');
  }
}

async function handleAttachmentChange(e) {
  const files = Array.from(e.target.files || []);
  const list = $('attach-list');
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
    });
    list.appendChild(el('li', { text: `${f.name} (${formatSize(f.size)})` }));
  }
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
  if (!state.currentMessage || !state.currentAccount) return;
  if (!confirm('Delete this message?')) return;
  try {
    await api(
      'DELETE',
      `/accounts/${state.currentAccount.id}/messages/${state.currentMessage.uid}?folder=${encodeURIComponent(state.currentFolder)}`
    );
    toast('Deleted', 'success');
    closeReader();
    loadMessages();
    loadFolders();
  } catch (e) { toast('Delete failed: ' + e.message, 'error'); }
}

async function markCurrentRead() {
  if (!state.currentMessage || !state.currentAccount) return;
  try {
    await api(
      'POST',
      `/accounts/${state.currentAccount.id}/messages/${state.currentMessage.uid}/flags`,
      { folder: state.currentFolder, add: ['seen'], remove: [] }
    );
    toast('Marked read', 'success');
    loadMessages();
    loadFolders();
  } catch (e) { toast('Flag failed: ' + e.message, 'error'); }
}

// ── Search ─────────────────────────────────────────────────────────
async function runSearch() {
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
  $('account-switcher').onchange = (e) => {
    const acct = state.accounts.find(a => a.id === e.target.value);
    if (acct) selectAccount(acct);
  };
  $('btn-refresh-folders').onclick = () => { loadFolders(); loadMessages(); };
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
}

// ── Boot ───────────────────────────────────────────────────────────
document.addEventListener('DOMContentLoaded', async () => {
  state.route = parseDashboardRoute(window.location.pathname);
  wireEvents();
  renderDrafts();
  await loadStats();
  await loadAccounts();
  setRefresh('ready');
});
