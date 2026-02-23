// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

const { useState, useReducer, useEffect, useRef, useCallback } = React;

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

function useInterval(callback, delay) {
  const saved = useRef(callback);
  useEffect(() => { saved.current = callback; }, [callback]);
  useEffect(() => {
    if (delay === null) return;
    const id = setInterval(() => saved.current(), delay);
    return () => clearInterval(id);
  }, [delay]);
}

async function api(url, opts = {}) {
  const r = await fetch(url, { headers: { 'Content-Type': 'application/json' }, ...opts });
  if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
  return r.json();
}

function relativeTime(iso) {
  if (!iso) return '';
  const diff = (Date.now() - new Date(iso).getTime()) / 1000;
  if (diff < 60) return 'just now';
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return `${Math.floor(diff / 86400)}d ago`;
}

// ---------------------------------------------------------------------------
// Variable highlighting (never uses dangerouslySetInnerHTML)
// ---------------------------------------------------------------------------

const HIGHLIGHT_PATTERNS = [
  /EUR\s?[\d,]+(?:k)?(?:\/\w+)?/g,
  /\$[\d,]+(?:k)?(?:\/\w+)?/g,
  /\b\d{1,3}(?:,\d{3})*(?:\.\d+)?%/g,
  /\bQ[1-4]\s?\d{4}\b/g,
  /\b\d{1,2}\/\d{1,2}(?:th|st|nd|rd)?\s+share\b/gi,
];

function highlightVariables(text) {
  const matches = [];
  for (const pattern of HIGHLIGHT_PATTERNS) {
    const p = new RegExp(pattern.source, pattern.flags);
    let m;
    while ((m = p.exec(text)) !== null) {
      matches.push({ start: m.index, end: m.index + m[0].length, text: m[0] });
    }
  }
  matches.sort((a, b) => a.start - b.start);
  const segments = [];
  let pos = 0;
  for (const m of matches) {
    if (m.start < pos) continue;
    if (m.start > pos) segments.push({ text: text.slice(pos, m.start), highlight: false });
    segments.push({ text: m.text, highlight: true });
    pos = m.end;
  }
  if (pos < text.length) segments.push({ text: text.slice(pos), highlight: false });
  return segments;
}

function HighlightedText({ text }) {
  if (!text) return null;
  const segments = highlightVariables(text);
  return (
    <span>
      {segments.map((seg, i) =>
        seg.highlight
          ? <mark key={i} className="bg-pending-light text-pending font-mono text-sm rounded px-0.5">{seg.text}</mark>
          : <span key={i}>{seg.text}</span>
      )}
    </span>
  );
}

// ---------------------------------------------------------------------------
// Per-card action state reducer
// ---------------------------------------------------------------------------

function actionReducer(state, action) {
  const { type, id } = action;
  const cur = state[id] || {
    busy: false, expanded: false,
    showRejectForm: false, showSendLater: false, showSnooze: false,
    feedback: '', sendAfterDate: '', snoozeUntilDate: '',
    threadData: null, threadLoading: false,
  };
  switch (type) {
    case 'SET_BUSY':          return { ...state, [id]: { ...cur, busy: action.busy } };
    case 'SET_EXPANDED':      return { ...state, [id]: { ...cur, expanded: action.expanded } };
    case 'TOGGLE_REJECT':     return { ...state, [id]: { ...cur, showRejectForm: !cur.showRejectForm, showSendLater: false, showSnooze: false } };
    case 'TOGGLE_SEND_LATER': return { ...state, [id]: { ...cur, showSendLater: !cur.showSendLater, showRejectForm: false, showSnooze: false } };
    case 'TOGGLE_SNOOZE':     return { ...state, [id]: { ...cur, showSnooze: !cur.showSnooze, showRejectForm: false, showSendLater: false } };
    case 'SET_FEEDBACK':      return { ...state, [id]: { ...cur, feedback: action.value } };
    case 'SET_SEND_AFTER':    return { ...state, [id]: { ...cur, sendAfterDate: action.value } };
    case 'SET_SNOOZE_UNTIL':  return { ...state, [id]: { ...cur, snoozeUntilDate: action.value } };
    case 'SET_THREAD':        return { ...state, [id]: { ...cur, threadData: action.data, threadLoading: false } };
    case 'SET_THREAD_LOADING':return { ...state, [id]: { ...cur, threadLoading: action.loading } };
    default: return state;
  }
}

// ---------------------------------------------------------------------------
// Signal chips
// ---------------------------------------------------------------------------

// Static class strings — Tailwind CDN play mode needs static strings
const CHIP_STYLES = {
  kb_match:   'text-xs px-1.5 py-0.5 rounded font-mono text-accent bg-accent-light',
  no_kb:      'text-xs px-1.5 py-0.5 rounded font-mono text-warn bg-warn-light',
  pricing:    'text-xs px-1.5 py-0.5 rounded font-mono text-pending bg-pending-light',
  legal:      'text-xs px-1.5 py-0.5 rounded font-mono text-pending bg-pending-light',
  scheduling: 'text-xs px-1.5 py-0.5 rounded font-mono text-pending bg-pending-light',
  no_thread:  'text-xs px-1.5 py-0.5 rounded font-mono text-mid border border-rule',
};

function SignalChips({ signals }) {
  if (!signals) return null;
  const chips = [];

  if (signals.kb_match === true) {
    chips.push({ key: 'kb_match', label: '✓ KB', style: CHIP_STYLES.kb_match });
  } else if (signals.kb_match === false) {
    chips.push({ key: 'no_kb', label: '✗ no KB', style: CHIP_STYLES.no_kb });
  }

  for (const cat of (signals.sensitive_categories || [])) {
    const style = CHIP_STYLES[cat] || CHIP_STYLES.scheduling;
    chips.push({ key: cat, label: `⚠ ${cat}`, style });
  }

  if (signals.thread_context === false) {
    chips.push({ key: 'no_thread', label: '✗ no thread', style: CHIP_STYLES.no_thread });
  }

  if (chips.length === 0) return null;
  return (
    <div className="flex items-center gap-1 flex-wrap">
      {chips.map(c => <span key={c.key} className={c.style}>{c.label}</span>)}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Threshold bar
// ---------------------------------------------------------------------------

function ThresholdBar({ account, onUpdateThresholds }) {
  const [autoVal, setAutoVal] = useState(account.auto_send_threshold ?? 0.85);
  const [reviewVal, setReviewVal] = useState(account.review_threshold ?? 0.50);
  const timerRef = useRef(null);

  // Sync with account prop changes (e.g. after successful PATCH)
  useEffect(() => {
    setAutoVal(account.auto_send_threshold ?? 0.85);
    setReviewVal(account.review_threshold ?? 0.50);
  }, [account.auto_send_threshold, account.review_threshold]);

  function handleChange(field, raw) {
    const num = parseFloat(raw);
    if (isNaN(num) || num < 0 || num > 1) return;
    if (field === 'auto_send_threshold') setAutoVal(num);
    else setReviewVal(num);
    clearTimeout(timerRef.current);
    timerRef.current = setTimeout(() => { onUpdateThresholds({ [field]: num }); }, 800);
  }

  useEffect(() => () => clearTimeout(timerRef.current), []);

  return (
    <div id="threshold-bar" className="flex flex-wrap items-center gap-4 px-4 py-3 border border-rule bg-white mb-4 text-sm">
      <span className="section-label">Policy</span>
      <label className="flex items-center gap-2">
        <span className="text-xs text-mid">Auto-send ≥</span>
        <input
          type="number" step="0.01" min="0" max="1"
          value={autoVal}
          onChange={e => handleChange('auto_send_threshold', e.target.value)}
          className="w-16 px-2 py-1 border border-rule text-xs font-mono text-ink bg-white rounded focus:outline-none focus:border-accent"
        />
      </label>
      <label className="flex items-center gap-2">
        <span className="text-xs text-mid">Review ≥</span>
        <input
          type="number" step="0.01" min="0" max="1"
          value={reviewVal}
          onChange={e => handleChange('review_threshold', e.target.value)}
          className="w-16 px-2 py-1 border border-rule text-xs font-mono text-ink bg-white rounded focus:outline-none focus:border-accent"
        />
      </label>
      <span className="text-xs text-mid">below → escalate</span>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Date/time picker inline panel (shared by Send later + Snooze)
// ---------------------------------------------------------------------------

function DateTimePicker({ label, value, onChange, onConfirm, onCancel }) {
  return (
    <div className="border-t border-rule p-3 bg-paper">
      <p className="text-xs font-medium text-ink mb-2">{label}</p>
      <input
        type="datetime-local"
        value={value}
        onChange={e => onChange(e.target.value)}
        className="input-field mb-2"
      />
      <div className="flex gap-2">
        <button onClick={onCancel} className="btn-ghost text-xs">Cancel</button>
        <button
          onClick={onConfirm}
          disabled={!value}
          className="btn-primary text-xs disabled:opacity-50"
        >
          Confirm →
        </button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Escalation card
// ---------------------------------------------------------------------------

function EscalationCard({ draft, accountId, actionState, dispatch, onDismiss }) {
  const meta = draft.metadata || {};
  const st = actionState || {};

  async function handleViewThread() {
    const mid = meta.inbound_message_id;
    if (!mid) return;
    dispatch({ type: 'SET_THREAD_LOADING', id: draft.id, loading: true });
    try {
      const data = await api(`/accounts/${accountId}/threads/${encodeURIComponent(mid)}`);
      dispatch({ type: 'SET_THREAD', id: draft.id, data });
    } catch {
      dispatch({ type: 'SET_THREAD', id: draft.id, data: { error: 'Failed to load thread' } });
    }
  }

  return (
    <div id={`card-${draft.id}`} className="card-escalation border border-warn rounded mb-3 bg-white overflow-hidden">
      <div className="p-4">
        <div className="flex items-start justify-between gap-4 mb-2">
          <div className="flex items-center gap-2">
            <span className="text-warn text-sm font-medium">⚠</span>
            <span className="text-sm font-medium text-ink">{draft.to_addr}</span>
          </div>
          <span className="text-xs font-mono text-mid">{(meta.confidence || 0).toFixed(2)}</span>
        </div>
        <p className="text-xs text-mid font-mono mb-2">{draft.subject}</p>
        <p className="text-sm text-ink leading-relaxed border-l-2 border-warn pl-3 mb-3">
          {meta.escalation_note || 'Requires human review.'}
        </p>
        <div className="flex items-center gap-2">
          <button
            onClick={handleViewThread}
            disabled={st.threadLoading || !meta.inbound_message_id}
            className="btn-ghost text-xs disabled:opacity-50"
          >
            {st.threadLoading ? 'Loading...' : 'View Thread ↓'}
          </button>
          <button onClick={() => onDismiss(draft.id)} className="btn-ghost text-xs">Dismiss</button>
        </div>
      </div>
      {st.threadData && (
        <div className="border-t border-rule p-4 bg-paper">
          {st.threadData.error
            ? <p className="text-xs text-warn">{st.threadData.error}</p>
            : (st.threadData.thread || []).map((msg, i) => (
              <div key={i} className="mb-3 last:mb-0 text-xs">
                <p className="font-medium text-ink">{msg.from_addr} <span className="text-mid font-normal">{msg.date}</span></p>
                <p className="text-mid mt-1 leading-relaxed">{(msg.text_body || '').slice(0, 400)}</p>
              </div>
            ))
          }
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Draft card (confidence-adaptive)
// ---------------------------------------------------------------------------

function DraftCard({
  draft, account, isSelected, onToggleSelect,
  onApprove, onSchedule, onSnooze, onReject,
  actionState, dispatch,
}) {
  const meta = draft.metadata || {};
  const confidence = typeof meta.confidence === 'number' ? meta.confidence : 0;
  const autoThresh = account.auto_send_threshold ?? 0.85;
  const reviewThresh = account.review_threshold ?? 0.50;
  const isAbove = confidence >= autoThresh;
  const isBetween = confidence >= reviewThresh && confidence < autoThresh;
  const st = actionState || {};

  // High-confidence: collapsed by default; between-threshold: expanded by default
  const expanded = st.expanded !== undefined ? st.expanded : isBetween;

  const confidenceClass = isAbove
    ? 'font-mono text-sm text-accent'
    : isBetween
      ? 'font-mono text-sm text-pending'
      : 'font-mono text-sm text-warn';

  const borderClass = isSelected
    ? 'border-accent'
    : isAbove
      ? 'border-rule'
      : 'border-pending';

  function handleSendLaterConfirm() {
    if (!st.sendAfterDate) return;
    const utc = new Date(st.sendAfterDate).toISOString();
    onSchedule(draft.id, utc);
    dispatch({ type: 'TOGGLE_SEND_LATER', id: draft.id });
  }

  function handleSnoozeConfirm() {
    if (!st.snoozeUntilDate) return;
    const utc = new Date(st.snoozeUntilDate).toISOString();
    onSnooze(draft.id, utc);
    dispatch({ type: 'TOGGLE_SNOOZE', id: draft.id });
  }

  return (
    <div id={`card-${draft.id}`} className={`card-draft border ${borderClass} rounded mb-3 bg-white overflow-hidden`}>
      <div className="p-4">
        {/* Header row */}
        <div className="flex items-start gap-3">
          {isAbove && (
            <input
              type="checkbox"
              checked={isSelected}
              onChange={() => onToggleSelect(draft.id)}
              className="mt-1 accent-accent"
            />
          )}
          <div className="flex-1 min-w-0">
            <div className="flex items-center justify-between gap-2 mb-1">
              <span className="text-sm font-medium text-ink truncate">{draft.to_addr}</span>
              <div className="flex items-center gap-2 shrink-0">
                <span className={confidenceClass}>{confidence.toFixed(2)}</span>
                <SignalChips signals={meta.signals} />
              </div>
            </div>
            <p className="text-xs text-mid font-mono mb-1">{draft.subject}</p>
            <p className="text-sm text-mid leading-snug">{meta.reasoning || ''}</p>
          </div>
        </div>

        {/* Draft body (collapsed/expanded) */}
        <div className="mt-3">
          {isAbove && !expanded && (
            <button
              onClick={() => dispatch({ type: 'SET_EXPANDED', id: draft.id, expanded: true })}
              className="text-xs text-mid hover:text-ink transition-colors"
            >
              ▶ Show draft
            </button>
          )}
          {expanded && draft.text_content && (
            <div className="border border-rule rounded p-3 bg-paper text-sm leading-relaxed text-ink whitespace-pre-wrap font-mono text-xs">
              {isBetween
                ? <HighlightedText text={draft.text_content} />
                : draft.text_content
              }
            </div>
          )}
          {expanded && isAbove && (
            <button
              onClick={() => dispatch({ type: 'SET_EXPANDED', id: draft.id, expanded: false })}
              className="text-xs text-mid hover:text-ink transition-colors mt-1"
            >
              ▲ Hide draft
            </button>
          )}
        </div>

        {/* CTA row */}
        <div className="mt-3 flex flex-wrap items-center gap-2">
          <button
            onClick={() => onApprove(draft.id)}
            disabled={st.busy}
            className="btn-primary disabled:opacity-50"
          >
            {st.busy ? 'Sending…' : 'Send →'}
          </button>

          {isBetween && (
            <button
              onClick={() => dispatch({ type: 'TOGGLE_SEND_LATER', id: draft.id })}
              className="btn-ghost"
            >
              Send later ↷
            </button>
          )}

          <button
            onClick={() => dispatch({ type: 'TOGGLE_SNOOZE', id: draft.id })}
            className="btn-ghost"
          >
            Snooze ⏱
          </button>

          {isBetween && (
            <button
              onClick={() => dispatch({ type: 'TOGGLE_REJECT', id: draft.id })}
              className="btn-warn"
            >
              Feedback ↺
            </button>
          )}

          {!isBetween && !isAbove && (
            <button
              onClick={() => dispatch({ type: 'TOGGLE_REJECT', id: draft.id })}
              className="btn-warn"
            >
              Feedback ↺
            </button>
          )}

          <span className="text-xs text-mid font-mono ml-auto">{relativeTime(draft.created_at)}</span>
        </div>

        {/* Scheduled badge */}
        {draft.send_after && (
          <div className="mt-2">
            <span className="text-xs px-2 py-0.5 rounded bg-accent-light text-accent font-mono">
              Scheduled for {new Date(draft.send_after).toLocaleString()}
            </span>
          </div>
        )}
      </div>

      {/* Inline Send later picker */}
      {st.showSendLater && (
        <DateTimePicker
          label="Send later?"
          value={st.sendAfterDate}
          onChange={v => dispatch({ type: 'SET_SEND_AFTER', id: draft.id, value: v })}
          onConfirm={handleSendLaterConfirm}
          onCancel={() => dispatch({ type: 'TOGGLE_SEND_LATER', id: draft.id })}
        />
      )}

      {/* Inline Snooze picker */}
      {st.showSnooze && (
        <DateTimePicker
          label="Snooze until?"
          value={st.snoozeUntilDate}
          onChange={v => dispatch({ type: 'SET_SNOOZE_UNTIL', id: draft.id, value: v })}
          onConfirm={handleSnoozeConfirm}
          onCancel={() => dispatch({ type: 'TOGGLE_SNOOZE', id: draft.id })}
        />
      )}

      {/* Inline reject/feedback form */}
      {st.showRejectForm && (
        <div className="border-t border-warn p-3 bg-warn-light">
          <p className="text-xs text-warn font-medium mb-2">Reject this draft?</p>
          <textarea
            value={st.feedback || ''}
            onChange={e => dispatch({ type: 'SET_FEEDBACK', id: draft.id, value: e.target.value })}
            placeholder="Tell the agent what to do differently (optional)"
            className="input-field text-xs mb-2"
            rows={2}
          />
          <div className="flex gap-2">
            <button
              onClick={() => dispatch({ type: 'TOGGLE_REJECT', id: draft.id })}
              className="btn-ghost text-xs"
            >
              Cancel
            </button>
            <button
              onClick={() => onReject(draft.id, st.feedback)}
              disabled={st.busy}
              className="btn-warn text-xs disabled:opacity-50"
            >
              {st.busy ? 'Rejecting…' : 'Confirm Reject'}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Batch approve bar
// ---------------------------------------------------------------------------

function BatchApproveBar({ count, onBatchApprove, busy }) {
  if (count === 0) return null;
  return (
    <div id="batch-approve-bar" className="flex items-center justify-between px-4 py-3 border border-accent rounded bg-accent-light mb-3">
      <span className="text-sm text-accent font-medium">{count} draft{count !== 1 ? 's' : ''} selected</span>
      <button
        onClick={onBatchApprove}
        disabled={busy}
        className="btn-primary disabled:opacity-50"
      >
        {busy ? `Approving ${count}…` : `Send ${count} →`}
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Account selector
// ---------------------------------------------------------------------------

function AccountSelector({ accounts, account, onSelect }) {
  if (accounts.length <= 1) return null;
  return (
    <div id="account-selector" className="mb-4">
      <select
        value={account?.id || ''}
        onChange={e => {
          const found = accounts.find(a => a.id === e.target.value);
          onSelect(found || null);
        }}
        className="input-field"
      >
        <option value="">Select account…</option>
        {accounts.map(a => (
          <option key={a.id} value={a.id}>{a.display_name || a.name} ({a.username})</option>
        ))}
      </select>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Queue list
// ---------------------------------------------------------------------------

function QueueList({
  drafts, account, selected, onToggleSelect,
  onApprove, onSchedule, onSnooze, onReject, onDismiss,
  actionStates, dispatch,
}) {
  if (drafts.length === 0) {
    return (
      <div id="queue-empty" className="border border-rule rounded px-6 py-12 text-center">
        <p className="text-mid text-sm">No drafts pending review.</p>
        <p className="text-mid text-xs font-mono mt-1">The agent hasn't created any drafts for this account, or all are snoozed.</p>
      </div>
    );
  }

  return (
    <div id="queue-list">
      {drafts.map(draft => {
        const meta = draft.metadata || {};
        if (meta.classification === 'escalate') {
          return (
            <EscalationCard
              key={draft.id}
              draft={draft}
              accountId={account.id}
              actionState={actionStates[draft.id]}
              dispatch={dispatch}
              onDismiss={onDismiss}
            />
          );
        }
        return (
          <DraftCard
            key={draft.id}
            draft={draft}
            account={account}
            isSelected={selected.has(draft.id)}
            onToggleSelect={onToggleSelect}
            onApprove={onApprove}
            onSchedule={onSchedule}
            onSnooze={onSnooze}
            onReject={onReject}
            actionState={actionStates[draft.id]}
            dispatch={dispatch}
          />
        );
      })}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Main app
// ---------------------------------------------------------------------------

function ReviewApp() {
  const [accounts, setAccounts] = useState([]);
  const [account, setAccount] = useState(null);
  const [drafts, setDrafts] = useState([]);
  const [selected, setSelected] = useState(new Set());
  const [actionStates, dispatch] = useReducer(actionReducer, {});
  const [batchBusy, setBatchBusy] = useState(false);
  const [error, setError] = useState(null);
  const [loading, setLoading] = useState(false);

  // ---------------------------------------------------------------------------
  // Data fetching
  // ---------------------------------------------------------------------------

  async function fetchAccounts() {
    try {
      const data = await api('/accounts');
      setAccounts(data);
      // Auto-select single account
      if (data.length === 1 && !account) {
        setAccount(data[0]);
      }
    } catch (e) {
      setError('Failed to load accounts: ' + e.message);
    }
  }

  async function fetchDrafts() {
    if (!account) return;
    setLoading(true);
    try {
      const data = await api(
        `/accounts/${account.id}/drafts?status=draft&created_by=inbox-agent&hide_snoozed=true&limit=100`
      );
      setDrafts(data);
    } catch (e) {
      setError('Failed to load drafts: ' + e.message);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { fetchAccounts(); }, []);
  useEffect(() => { if (account) fetchDrafts(); }, [account?.id]);
  useInterval(fetchDrafts, 30000);

  // ---------------------------------------------------------------------------
  // Actions
  // ---------------------------------------------------------------------------

  function removeDraft(draftId) {
    setDrafts(prev => prev.filter(d => d.id !== draftId));
    setSelected(prev => { const n = new Set(prev); n.delete(draftId); return n; });
  }

  async function handleApprove(draftId) {
    dispatch({ type: 'SET_BUSY', id: draftId, busy: true });
    try {
      await api(`/accounts/${account.id}/drafts/${draftId}/approve`, { method: 'POST' });
      removeDraft(draftId);
    } catch (e) {
      setError('Send failed: ' + e.message);
    } finally {
      dispatch({ type: 'SET_BUSY', id: draftId, busy: false });
    }
  }

  async function handleSchedule(draftId, sendAfter) {
    dispatch({ type: 'SET_BUSY', id: draftId, busy: true });
    try {
      await api(`/accounts/${account.id}/drafts/${draftId}`, {
        method: 'PATCH',
        body: JSON.stringify({ send_after: sendAfter }),
      });
      removeDraft(draftId);
    } catch (e) {
      setError('Schedule failed: ' + e.message);
    } finally {
      dispatch({ type: 'SET_BUSY', id: draftId, busy: false });
    }
  }

  async function handleSnooze(draftId, snoozedUntil) {
    dispatch({ type: 'SET_BUSY', id: draftId, busy: true });
    try {
      await api(`/accounts/${account.id}/drafts/${draftId}`, {
        method: 'PATCH',
        body: JSON.stringify({ snoozed_until: snoozedUntil }),
      });
      removeDraft(draftId);
    } catch (e) {
      setError('Snooze failed: ' + e.message);
    } finally {
      dispatch({ type: 'SET_BUSY', id: draftId, busy: false });
    }
  }

  async function handleReject(draftId, feedback) {
    dispatch({ type: 'SET_BUSY', id: draftId, busy: true });
    try {
      await api(`/accounts/${account.id}/drafts/${draftId}/reject`, {
        method: 'POST',
        body: JSON.stringify({ feedback: feedback || null }),
      });
      removeDraft(draftId);
    } catch (e) {
      setError('Reject failed: ' + e.message);
    } finally {
      dispatch({ type: 'SET_BUSY', id: draftId, busy: false });
    }
  }

  async function handleDismiss(draftId) {
    await handleReject(draftId, 'dismissed');
  }

  async function handleBatchApprove() {
    const ids = Array.from(selected);
    setBatchBusy(true);
    try {
      await Promise.all(ids.map(id =>
        api(`/accounts/${account.id}/drafts/${id}/approve`, { method: 'POST' })
      ));
      ids.forEach(id => removeDraft(id));
    } catch (e) {
      setError('Batch approve partially failed: ' + e.message);
      await fetchDrafts();
    } finally {
      setBatchBusy(false);
    }
  }

  async function handleUpdateThresholds(updates) {
    try {
      const updated = await api(`/accounts/${account.id}`, {
        method: 'PATCH',
        body: JSON.stringify(updates),
      });
      setAccount(updated);
      setAccounts(prev => prev.map(a => a.id === updated.id ? updated : a));
    } catch (e) {
      setError('Failed to update thresholds: ' + e.message);
    }
  }

  function toggleSelect(id) {
    setSelected(prev => {
      const next = new Set(prev);
      next.has(id) ? next.delete(id) : next.add(id);
      return next;
    });
  }

  // ---------------------------------------------------------------------------
  // Render
  // ---------------------------------------------------------------------------

  const aboveThresholdCount = drafts.filter(d => {
    const meta = d.metadata || {};
    const conf = typeof meta.confidence === 'number' ? meta.confidence : 0;
    return conf >= (account?.auto_send_threshold ?? 0.85);
  }).length;

  const selectableIds = new Set(
    drafts
      .filter(d => {
        const meta = d.metadata || {};
        const conf = typeof meta.confidence === 'number' ? meta.confidence : 0;
        return conf >= (account?.auto_send_threshold ?? 0.85);
      })
      .map(d => d.id)
  );

  return (
    <div>
      {error && (
        <div id="error-banner" className="mb-4 px-4 py-3 border border-warn rounded bg-warn-light text-warn text-sm flex items-center justify-between">
          <span>{error}</span>
          <button onClick={() => setError(null)} className="text-warn hover:opacity-70 ml-4 font-mono">✕</button>
        </div>
      )}

      <AccountSelector
        accounts={accounts}
        account={account}
        onSelect={acc => { setAccount(acc); setDrafts([]); setSelected(new Set()); }}
      />

      {!account && accounts.length === 0 && (
        <div className="text-center py-16 text-mid text-sm">
          <p>No accounts configured.</p>
          <a href="/" className="text-accent hover:underline text-xs mt-2 inline-block">Set up an account →</a>
        </div>
      )}

      {account && (
        <>
          <ThresholdBar account={account} onUpdateThresholds={handleUpdateThresholds} />

          <div id="queue-header" className="flex items-center justify-between mb-3">
            <div className="flex items-center gap-3">
              <h2 className="section-label">
                {loading ? 'Loading…' : `${drafts.length} draft${drafts.length !== 1 ? 's' : ''} pending`}
              </h2>
              {aboveThresholdCount > 0 && (
                <button
                  onClick={() => setSelected(selectableIds)}
                  className="text-xs text-mid hover:text-ink font-mono transition-colors"
                >
                  Select all above auto-send ({aboveThresholdCount})
                </button>
              )}
            </div>
            <button
              onClick={fetchDrafts}
              className="text-xs text-mid hover:text-ink font-mono transition-colors"
            >
              ↺ Refresh
            </button>
          </div>

          <BatchApproveBar
            count={selected.size}
            onBatchApprove={handleBatchApprove}
            busy={batchBusy}
          />

          <QueueList
            drafts={drafts}
            account={account}
            selected={selected}
            onToggleSelect={toggleSelect}
            onApprove={handleApprove}
            onSchedule={handleSchedule}
            onSnooze={handleSnooze}
            onReject={handleReject}
            onDismiss={handleDismiss}
            actionStates={actionStates}
            dispatch={dispatch}
          />
        </>
      )}
    </div>
  );
}

const root = ReactDOM.createRoot(document.getElementById('review-root'));
root.render(<ReviewApp />);
