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
  snoozed: [],
  composeMode: 'new',
  composeParent: null,
  pendingAttachments: [],
  bodyFormat: 'text',
  showAllAccounts: false,
};

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

function setRefresh(text) { $('last-refresh').textContent = text; }

// ── Stats ──────────────────────────────────────────────────────────
async function loadStats() {
  try {
    const stats = await api('GET', '/stats');
    $('stat-accounts').textContent = stats.accounts ?? 0;
    $('stat-snoozed').textContent = stats.snoozed ?? 0;
  } catch (e) { console.error('loadStats', e); }
}

// ── Accounts ───────────────────────────────────────────────────────
async function loadAccounts() {
  try {
    const data = await api('GET', '/accounts');
    state.accounts = data.accounts || [];
    renderAccountSwitcher();
    renderAccountsList();
    if (!state.currentAccount && state.accounts.length > 0) {
      selectAccount(state.accounts[0]);
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
  renderAccountSwitcher();
  // Sequential — folders first (creates the IMAP connection), then messages reuse it.
  await loadFolders();
  await loadMessages();
}

// ── Folders ────────────────────────────────────────────────────────
async function loadFolders() {
  if (!state.currentAccount) return;
  setRefresh('loading folders…');
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
  } catch (e) {
    clear(list);
    const retry = el('button', { class: 'btn-ghost text-xs mt-2', text: 'Retry' });
    retry.onclick = loadFolders;
    list.appendChild(el('div', { class: 'px-3 py-4 text-xs text-warn font-mono', text: 'Failed to load folders' }));
    list.appendChild(retry);
    setRefresh('error');
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
    const unseenLabel = f.unseen && f.unseen > 0 ? `${f.unseen}/${f.exists}` : `${f.exists}`;
    item.appendChild(el('span', { class: 'name', text: f.folder }));
    item.appendChild(el('span', { class: 'count', text: unseenLabel }));
    item.onclick = () => {
      state.currentFolder = f.folder;
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
  const folderLabel = state.currentFolder === '__snoozed__' ? '★ Snoozed' : state.currentFolder;
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
    $('list-count').textContent = `${state.messages.length} message${state.messages.length === 1 ? '' : 's'}`;
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

// ── Composer ───────────────────────────────────────────────────────
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
  if (!q) { loadMessages(); return; }
  setRefresh('searching…');
  try {
    const data = await api(
      'GET',
      `/accounts/${state.currentAccount.id}/search?q=${encodeURIComponent(q)}&folder=${encodeURIComponent(state.currentFolder)}&limit=100`
    );
    state.messages = data.messages || [];
    renderMessages();
    $('list-count').textContent = `${state.messages.length} results`;
    setRefresh('ok');
  } catch (e) {
    setRefresh('error');
    toast('Search: ' + e.message, 'error');
  }
}

// ── Event wiring ───────────────────────────────────────────────────
function wireEvents() {
  $('account-switcher').onchange = (e) => {
    const acct = state.accounts.find(a => a.id === e.target.value);
    if (acct) selectAccount(acct);
  };
  $('btn-refresh-folders').onclick = () => { loadFolders(); loadMessages(); };
  $('btn-add-account').onclick = openAddAccount;
  $('btn-add-account-close').onclick = closeAddAccount;
  $('btn-discover').onclick = runDiscover;
  $('btn-create-account').onclick = createAccount;

  $('btn-compose').onclick = () => openComposer('new');
  $('btn-composer-close').onclick = closeComposer;
  $('btn-composer-send').onclick = sendComposer;
  $('composer-attach').onchange = handleAttachmentChange;

  $('format-text').onclick = () => {
    state.bodyFormat = 'text';
    $('format-text').className = 'px-2 py-0.5 text-xs font-mono border border-ink bg-ink text-paper';
    $('format-html').className = 'px-2 py-0.5 text-xs font-mono border border-rule text-mid';
  };
  $('format-html').onclick = () => {
    state.bodyFormat = 'html';
    $('format-html').className = 'px-2 py-0.5 text-xs font-mono border border-ink bg-ink text-paper';
    $('format-text').className = 'px-2 py-0.5 text-xs font-mono border border-rule text-mid';
  };

  $('btn-reader-close').onclick = closeReader;
  $('btn-reader-reply').onclick = () => openComposer('reply', state.currentMessage);
  $('btn-reader-reply-all').onclick = () => openComposer('reply-all', state.currentMessage);
  $('btn-reader-delete').onclick = deleteCurrentMessage;
  $('btn-reader-mark-read').onclick = markCurrentRead;
  $('btn-reader-snooze').onclick = openSnoozeModal;

  $('btn-snooze-cancel').onclick = closeSnoozeModal;
  $('btn-snooze-confirm').onclick = () => {
    toast('Snooze endpoint not yet wired in v0.3.0 dashboard — use CLI: envelope snooze set <uid> --until ...', 'error');
    closeSnoozeModal();
  };

  $('btn-search').onclick = runSearch;
  $('search-input').onkeydown = (e) => { if (e.key === 'Enter') runSearch(); };
}

// ── Boot ───────────────────────────────────────────────────────────
document.addEventListener('DOMContentLoaded', async () => {
  wireEvents();
  await loadStats();
  await loadAccounts();
  setRefresh('ready');
});
