// Envelope Dashboard — all fetch/render logic

(function () {
  'use strict';

  const REFRESH_INTERVAL = 30000;
  let currentFormat = 'text';
  let accounts = [];

  // ── Helpers ──

  function relativeTime(iso) {
    if (!iso) return '--';
    const now = Date.now();
    const then = new Date(iso).getTime();
    const diff = Math.max(0, now - then);
    const seconds = Math.floor(diff / 1000);
    if (seconds < 10) return 'just now';
    if (seconds < 60) return seconds + 's ago';
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return minutes + 'm ago';
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return hours + 'h ago';
    const days = Math.floor(hours / 24);
    return days + 'd ago';
  }

  function esc(str) {
    if (!str) return '';
    const el = document.createElement('span');
    el.textContent = str;
    return el.innerHTML;
  }

  function statusBadge(status) {
    const map = {
      sent: 'bg-accent-light text-accent',
      failed: 'bg-warn-light text-warn',
      queued: 'bg-pending-light text-pending',
      retry: 'bg-amber-50 text-amber-700',
      sending: 'bg-blue-50 text-blue-700',
    };
    const cls = map[status] || 'bg-neutral-100 text-mid';
    return '<span class="inline-block px-2 py-0.5 text-[11px] font-mono font-medium rounded-sm ' + cls + '">' + esc(status) + '</span>';
  }

  async function api(path, opts) {
    const res = await fetch(path, opts);
    return res.json();
  }

  // ── Stats ──

  async function loadStats() {
    const stats = await api('/stats');
    document.getElementById('stat-total').textContent = stats.total;
    document.getElementById('stat-sent').textContent = stats.sent;
    document.getElementById('stat-failed').textContent = stats.failed;
    document.getElementById('stat-rate').textContent = stats.total > 0
      ? stats.success_rate + '%'
      : '--';
  }

  // ── Accounts ──

  async function loadAccounts() {
    accounts = await api('/accounts');
    renderAccounts();
    renderAccountDropdown();
  }

  function renderAccounts() {
    const container = document.getElementById('accounts-list');

    if (accounts.length === 0) {
      container.innerHTML = '<div class="px-4 py-8 text-center text-sm text-mid">No accounts configured. Add one above.</div>';
      return;
    }

    container.innerHTML = accounts.map(function (a) {
      const verified = a.verified_at
        ? '<span class="text-accent text-[11px] font-mono">verified</span>'
        : '<span class="text-mid text-[11px] font-mono">unverified</span>';

      return '<div class="account-row flex items-center justify-between px-4 py-3 bg-white hover:bg-neutral-50/50 transition-colors" data-id="' + esc(a.id) + '">'
        + '<div class="min-w-0 flex-1">'
        + '<p class="text-sm font-medium truncate">' + esc(a.name) + '</p>'
        + '<p class="text-xs text-mid font-mono mt-0.5 truncate">' + esc(a.username) + ' / ' + esc(a.smtp_host) + ':' + a.smtp_port + '</p>'
        + '</div>'
        + '<div class="flex items-center gap-2 ml-4 flex-shrink-0">'
        + verified
        + '<button class="btn-verify btn-ghost text-[11px]" data-id="' + esc(a.id) + '">Verify</button>'
        + '<button class="btn-delete text-[11px] px-2 py-1 text-mid hover:text-warn transition-colors" data-id="' + esc(a.id) + '">Delete</button>'
        + '</div>'
        + '</div>';
    }).join('');

    container.querySelectorAll('.btn-verify').forEach(function (btn) {
      btn.addEventListener('click', function () { verifyAccount(btn.dataset.id); });
    });
    container.querySelectorAll('.btn-delete').forEach(function (btn) {
      btn.addEventListener('click', function () { deleteAccount(btn.dataset.id); });
    });
  }

  function renderAccountDropdown() {
    const select = document.getElementById('send-account');
    const current = select.value;
    select.innerHTML = '<option value="">Select account...</option>';
    accounts.forEach(function (a) {
      const opt = document.createElement('option');
      opt.value = a.id;
      opt.textContent = a.name + ' (' + a.username + ')';
      select.appendChild(opt);
    });
    if (current) select.value = current;
  }

  async function verifyAccount(id) {
    const btn = document.querySelector('.btn-verify[data-id="' + id + '"]');
    if (btn) { btn.textContent = '...'; btn.disabled = true; }
    try {
      const result = await api('/accounts/' + id + '/verify', { method: 'POST' });
      if (result.verification && result.verification.smtp && result.verification.smtp.status === 'ok') {
        await loadAccounts();
      } else {
        var msg = (result.verification && result.verification.smtp && result.verification.smtp.message) || 'Verification failed';
        if (btn) { btn.textContent = 'Failed'; }
        setTimeout(function () { if (btn) { btn.textContent = 'Verify'; btn.disabled = false; } }, 2000);
      }
    } catch (e) {
      if (btn) { btn.textContent = 'Error'; btn.disabled = false; }
    }
  }

  async function deleteAccount(id) {
    var acct = accounts.find(function (a) { return a.id === id; });
    var name = acct ? acct.name : id;
    if (!confirm('Delete account "' + name + '"?')) return;
    await api('/accounts/' + id, { method: 'DELETE' });
    await loadAccounts();
  }

  // ── Add Account ──

  function setupAddAccount() {
    var form = document.getElementById('add-account-form');
    var toggleBtn = document.getElementById('toggle-add-account');
    var saveBtn = document.getElementById('btn-save-account');
    var cancelBtn = document.getElementById('btn-cancel-account');
    var discoverBtn = document.getElementById('btn-discover');
    var errorEl = document.getElementById('add-account-error');

    toggleBtn.addEventListener('click', function () {
      form.classList.toggle('hidden');
      toggleBtn.textContent = form.classList.contains('hidden') ? '+ Add' : 'Close';
    });

    cancelBtn.addEventListener('click', function () {
      form.classList.add('hidden');
      toggleBtn.textContent = '+ Add';
      clearAddForm();
    });

    discoverBtn.addEventListener('click', runDiscover);

    saveBtn.addEventListener('click', async function () {
      errorEl.classList.add('hidden');
      var smtpHost = document.getElementById('acc-smtp-host').value.trim();
      var imapHost = document.getElementById('acc-imap-host').value.trim();
      var payload = {
        name: document.getElementById('acc-name').value.trim(),
        smtp_host: smtpHost,
        imap_host: imapHost,
        smtp_port: parseInt(document.getElementById('acc-smtp-port').value) || 587,
        imap_port: parseInt(document.getElementById('acc-imap-port').value) || 993,
        username: document.getElementById('acc-username').value.trim(),
        password: document.getElementById('acc-password').value,
        display_name: document.getElementById('acc-display-name').value.trim() || undefined,
      };

      if (!payload.name || !smtpHost || !imapHost || !payload.username || !payload.password) {
        errorEl.textContent = 'Name, hosts, username, and password are required.';
        errorEl.classList.remove('hidden');
        return;
      }

      saveBtn.textContent = 'Saving...';
      saveBtn.disabled = true;

      try {
        var res = await fetch('/accounts', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(payload),
        });
        if (!res.ok) {
          var err = await res.json();
          throw new Error(err.detail || 'Failed to create account');
        }
        form.classList.add('hidden');
        toggleBtn.textContent = '+ Add';
        clearAddForm();
        await loadAccounts();
      } catch (e) {
        errorEl.textContent = e.message;
        errorEl.classList.remove('hidden');
      } finally {
        saveBtn.textContent = 'Save Account';
        saveBtn.disabled = false;
      }
    });
  }

  function runDiscover() {
    var email = document.getElementById('acc-username').value.trim();
    var statusEl = document.getElementById('discover-status');
    var discoverBtn = document.getElementById('btn-discover');

    if (!email || !email.includes('@')) {
      statusEl.textContent = 'Enter an email address first.';
      statusEl.className = 'text-xs font-mono mt-1 text-warn';
      statusEl.classList.remove('hidden');
      return;
    }

    discoverBtn.textContent = 'Discovering...';
    discoverBtn.disabled = true;
    statusEl.textContent = 'Starting discovery...';
    statusEl.className = 'text-xs font-mono mt-1 text-mid';
    statusEl.classList.remove('hidden');

    var source = new EventSource('/accounts/discover/stream?email=' + encodeURIComponent(email));

    source.addEventListener('phase', function (e) {
      try {
        var d = JSON.parse(e.data);
        statusEl.textContent = d.message || ('Phase: ' + d.name);
        statusEl.className = 'text-xs font-mono mt-1 text-mid';
      } catch (err) { /* ignore parse errors */ }
    });

    source.addEventListener('complete', function (e) {
      source.close();
      discoverBtn.textContent = 'Discover';
      discoverBtn.disabled = false;

      try {
        var data = JSON.parse(e.data);

        if (data.error) {
          statusEl.textContent = data.error;
          statusEl.className = 'text-xs font-mono mt-1 text-warn';
          return;
        }

        var found = [];
        if (data.smtp_host) {
          document.getElementById('acc-smtp-host').value = data.smtp_host;
          document.getElementById('acc-smtp-port').value = data.smtp_port;
          found.push('SMTP: ' + data.smtp_host + ':' + data.smtp_port + ' (' + data.smtp_source + ')');
        }
        if (data.imap_host) {
          document.getElementById('acc-imap-host').value = data.imap_host;
          document.getElementById('acc-imap-port').value = data.imap_port;
          found.push('IMAP: ' + data.imap_host + ':' + data.imap_port + ' (' + data.imap_source + ')');
        }

        if (found.length > 0) {
          statusEl.innerHTML = found.join('<br>');
          statusEl.className = 'text-xs font-mono mt-1 text-accent';
        } else {
          statusEl.textContent = 'No servers found. Enter settings manually.';
          statusEl.className = 'text-xs font-mono mt-1 text-warn';
        }
      } catch (err) {
        statusEl.textContent = 'Discovery failed: ' + err.message;
        statusEl.className = 'text-xs font-mono mt-1 text-warn';
      }
    });

    source.addEventListener('error', function () {
      source.close();
      discoverBtn.textContent = 'Discover';
      discoverBtn.disabled = false;
      statusEl.textContent = 'Discovery connection lost.';
      statusEl.className = 'text-xs font-mono mt-1 text-warn';
    });
  }

  function clearAddForm() {
    ['acc-name', 'acc-smtp-host', 'acc-imap-host', 'acc-username', 'acc-password', 'acc-display-name'].forEach(function (id) {
      document.getElementById(id).value = '';
    });
    document.getElementById('acc-smtp-port').value = '587';
    document.getElementById('acc-imap-port').value = '993';
    document.getElementById('add-account-error').classList.add('hidden');
    document.getElementById('discover-status').classList.add('hidden');
  }

  // ── Messages ──

  async function loadMessages() {
    var msgs = await api('/messages?limit=50');
    renderMessages(msgs);
  }

  function renderMessages(msgs) {
    var body = document.getElementById('messages-body');
    var countEl = document.getElementById('msg-count');
    countEl.textContent = msgs.length + ' messages';

    if (msgs.length === 0) {
      body.innerHTML = '<tr><td colspan="5" class="px-4 py-8 text-center text-sm text-mid">No messages yet. Send one above.</td></tr>';
      return;
    }

    body.innerHTML = msgs.map(function (m) {
      return '<tr class="hover:bg-neutral-50/50 transition-colors">'
        + '<td class="px-4 py-2.5">' + statusBadge(m.status) + '</td>'
        + '<td class="px-4 py-2.5 font-mono text-xs truncate max-w-[160px]">' + esc(m.to_addr) + '</td>'
        + '<td class="px-4 py-2.5 text-xs truncate max-w-[200px]">' + esc(m.subject || '(no subject)') + '</td>'
        + '<td class="px-4 py-2.5 font-mono text-xs text-mid truncate max-w-[140px] hidden md:table-cell">' + esc(m.from_addr) + '</td>'
        + '<td class="px-4 py-2.5 text-right text-xs text-mid font-mono whitespace-nowrap">' + relativeTime(m.sent_at || m.created_at) + '</td>'
        + '</tr>';
    }).join('');
  }

  // ── Send ──

  function setupSend() {
    var textBtn = document.getElementById('toggle-text');
    var htmlBtn = document.getElementById('toggle-html');
    var bodyEl = document.getElementById('send-body');
    var sendBtn = document.getElementById('btn-send');
    var statusEl = document.getElementById('send-status');

    textBtn.addEventListener('click', function () {
      currentFormat = 'text';
      textBtn.className = 'px-2 py-0.5 text-xs font-mono border border-ink bg-ink text-paper transition-colors';
      htmlBtn.className = 'px-2 py-0.5 text-xs font-mono border border-rule text-mid hover:border-ink hover:text-ink transition-colors';
      bodyEl.placeholder = 'Message body';
    });

    htmlBtn.addEventListener('click', function () {
      currentFormat = 'html';
      htmlBtn.className = 'px-2 py-0.5 text-xs font-mono border border-ink bg-ink text-paper transition-colors';
      textBtn.className = 'px-2 py-0.5 text-xs font-mono border border-rule text-mid hover:border-ink hover:text-ink transition-colors';
      bodyEl.placeholder = '<html> body content';
    });

    sendBtn.addEventListener('click', async function () {
      var accountId = document.getElementById('send-account').value;
      var to = document.getElementById('send-to').value.trim();
      var subject = document.getElementById('send-subject').value.trim();
      var body = bodyEl.value;

      if (!accountId || !to || !subject) {
        statusEl.textContent = 'Account, To, and Subject are required.';
        statusEl.className = 'text-xs font-mono text-warn';
        return;
      }

      var payload = {
        account_id: accountId,
        to: to,
        subject: subject,
      };
      if (currentFormat === 'html') {
        payload.html = body;
      } else {
        payload.text = body;
      }

      sendBtn.textContent = 'Sending...';
      sendBtn.disabled = true;
      statusEl.textContent = '';

      try {
        var res = await fetch('/send', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(payload),
        });
        var data = await res.json();

        if (res.ok && data.status === 'sent') {
          statusEl.textContent = 'Sent. ID: ' + (data.id || '').slice(0, 8);
          statusEl.className = 'text-xs font-mono text-accent';
          document.getElementById('send-to').value = '';
          document.getElementById('send-subject').value = '';
          bodyEl.value = '';
          await Promise.all([loadMessages(), loadStats()]);
        } else {
          statusEl.textContent = data.error || data.detail || 'Send failed';
          statusEl.className = 'text-xs font-mono text-warn';
        }
      } catch (e) {
        statusEl.textContent = 'Network error: ' + e.message;
        statusEl.className = 'text-xs font-mono text-warn';
      } finally {
        sendBtn.textContent = 'Send';
        sendBtn.disabled = false;
      }
    });
  }

  // ── Refresh Loop ──

  function updateRefreshTime() {
    var el = document.getElementById('last-refresh');
    el.textContent = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  }

  async function refreshAll() {
    try {
      await Promise.all([loadStats(), loadAccounts(), loadMessages()]);
      updateRefreshTime();
    } catch (e) {
      console.error('Refresh failed:', e);
    }
  }

  // ── Init ──

  document.addEventListener('DOMContentLoaded', function () {
    setupAddAccount();
    setupSend();
    refreshAll();
    setInterval(refreshAll, REFRESH_INTERVAL);
  });

})();
