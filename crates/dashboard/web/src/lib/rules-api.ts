// Rules API client — additive to api.ts; do NOT modify api.ts.
//
// Covers the rules control plane:  list, create, update, delete, enable,
// disable, preview.  Action validation mirrors what the backend accepts —
// the Action enum variants from crates/email/src/rules.rs.

import { request, type RequestOptions } from './api';

// ── Domain types ──────────────────────────────────────────────────────

/** A rule stored in the database.  Shape mirrors `dashboard_rule_json()`. */
export interface Rule {
  id: string;
  account_id: string;
  name: string;
  /** Raw JSON string of the MatchExpr. */
  match_expr: string;
  /** Raw JSON string of the Action (webhook URL is redacted by the server). */
  action: string;
  enabled: boolean;
  priority: number;
  stop: boolean;
  sieve_exportable: boolean;
  hit_count: number;
  last_hit_at: string | null;
  created_at: string;
  updated_at: string;
}

export interface RulesListResponse {
  rules: Rule[];
}

export interface RuleResponse {
  rule: Rule;
}

// ── Match expression building blocks ─────────────────────────────────

/** Simple single-field match conditions surfaced in the UI. */
export interface MatchFields {
  from?: string;
  to?: string;
  subject?: string;
  tag?: string;
  /** "dimension=threshold" pair, e.g. "spam=0.8" */
  scoreAbove?: string;
  scoreBelow?: string;
}

/**
 * Build a MatchExpr JSON value from the form fields.
 *
 * Multiple non-empty fields are AND'd together.  If only one field is set,
 * the expression is the leaf node directly (no redundant And wrapper).
 * Returns null when no fields are provided (caller must validate).
 */
export function buildMatchExpr(f: MatchFields): unknown | null {
  const conditions: unknown[] = [];

  if (f.from?.trim()) conditions.push({ from: f.from.trim() });
  if (f.to?.trim()) conditions.push({ to: f.to.trim() });
  if (f.subject?.trim()) conditions.push({ subject: f.subject.trim() });
  if (f.tag?.trim()) conditions.push({ has_tag: f.tag.trim() });

  if (f.scoreAbove?.trim()) {
    const [dim, val] = f.scoreAbove.trim().split('=', 2);
    const threshold = parseFloat(val ?? '');
    if (dim && !isNaN(threshold)) {
      conditions.push({ score_above: { dimension: dim.trim(), threshold } });
    }
  }
  if (f.scoreBelow?.trim()) {
    const [dim, val] = f.scoreBelow.trim().split('=', 2);
    const threshold = parseFloat(val ?? '');
    if (dim && !isNaN(threshold)) {
      conditions.push({ score_below: { dimension: dim.trim(), threshold } });
    }
  }

  if (conditions.length === 0) return null;
  if (conditions.length === 1) return conditions[0];
  return { and: conditions };
}

/**
 * Parse a stored match_expr JSON string back into MatchFields for the editor.
 *
 * Only the single-leaf and And-of-leaves shapes are round-trippable through
 * the simple field UI.  Complex nested expressions are left as raw JSON for
 * the user to see but not re-edit via fields.
 */
export function parseMatchExpr(raw: string): MatchFields & { _raw?: string } {
  let expr: unknown;
  try {
    expr = JSON.parse(raw);
  } catch {
    return { _raw: raw };
  }

  const leaves = collectLeaves(expr);
  if (leaves === null) return { _raw: raw };

  const fields: MatchFields = {};
  for (const leaf of leaves) {
    if (leaf && typeof leaf === 'object') {
      const obj = leaf as Record<string, unknown>;
      if (typeof obj.from === 'string') fields.from = obj.from;
      else if (typeof obj.to === 'string') fields.to = obj.to;
      else if (typeof obj.subject === 'string') fields.subject = obj.subject;
      else if (typeof obj.has_tag === 'string') fields.tag = obj.has_tag;
      else if (obj.score_above && typeof obj.score_above === 'object') {
        const sa = obj.score_above as { dimension?: string; threshold?: number };
        if (sa.dimension && sa.threshold !== undefined)
          fields.scoreAbove = `${sa.dimension}=${sa.threshold}`;
      } else if (obj.score_below && typeof obj.score_below === 'object') {
        const sb = obj.score_below as { dimension?: string; threshold?: number };
        if (sb.dimension && sb.threshold !== undefined)
          fields.scoreBelow = `${sb.dimension}=${sb.threshold}`;
      } else {
        // Unrecognised leaf shape — fall through to raw mode.
        return { _raw: raw };
      }
    }
  }
  return fields;
}

/** Returns the array of leaf nodes from an And-of-leaves or a single leaf.
 *  Returns null for any shape too complex for the simple field UI. */
function collectLeaves(expr: unknown): unknown[] | null {
  if (!expr || typeof expr !== 'object') return null;
  const obj = expr as Record<string, unknown>;
  if (Array.isArray(obj.and)) {
    // All children must be simple leaves (no nesting).
    const leaves = obj.and as unknown[];
    for (const leaf of leaves) {
      if (leaf && typeof leaf === 'object' && 'and' in (leaf as object)) return null;
      if (leaf && typeof leaf === 'object' && 'or' in (leaf as object)) return null;
      if (leaf && typeof leaf === 'object' && 'not' in (leaf as object)) return null;
    }
    return leaves;
  }
  // Single leaf.
  if ('and' in obj || 'or' in obj || 'not' in obj) return null;
  return [obj];
}

// ── Action building blocks ────────────────────────────────────────────

export type ActionKind =
  | 'move'
  | 'flag'
  | 'unflag'
  | 'delete'
  | 'unsubscribe'
  | 'add_tag'
  | 'snooze'
  | 'webhook';

export interface ActionFields {
  kind: ActionKind;
  /** Required for move, flag, unflag, add_tag, snooze, webhook. */
  arg?: string;
}

/** Build an Action JSON value from the editor fields. */
export function buildAction(f: ActionFields): unknown {
  switch (f.kind) {
    case 'delete':
      return 'delete';
    case 'unsubscribe':
      return 'unsubscribe';
    case 'move':
      return { move: f.arg ?? '' };
    case 'flag':
      return { flag: f.arg ?? '' };
    case 'unflag':
      return { unflag: f.arg ?? '' };
    case 'add_tag':
      return { add_tag: f.arg ?? '' };
    case 'snooze':
      return { snooze: f.arg ?? '' };
    case 'webhook':
      return { webhook: f.arg ?? '' };
  }
}

/**
 * Parse a stored action JSON string into ActionFields.
 *
 * The server redacts webhook URLs as `[redacted]`, so when editing a rule
 * with a webhook action the user must re-enter the URL.
 */
export function parseAction(raw: string): ActionFields {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return { kind: 'move', arg: '' };
  }

  if (parsed === 'delete') return { kind: 'delete' };
  if (parsed === 'unsubscribe') return { kind: 'unsubscribe' };

  if (parsed && typeof parsed === 'object') {
    const obj = parsed as Record<string, unknown>;
    if (typeof obj.move === 'string') return { kind: 'move', arg: obj.move };
    if (typeof obj.flag === 'string') return { kind: 'flag', arg: obj.flag };
    if (typeof obj.unflag === 'string') return { kind: 'unflag', arg: obj.unflag };
    if (typeof obj.add_tag === 'string') return { kind: 'add_tag', arg: obj.add_tag };
    if (typeof obj.snooze === 'string') return { kind: 'snooze', arg: obj.snooze };
    if (typeof obj.webhook === 'string')
      return { kind: 'webhook', arg: obj.webhook === '[redacted]' ? '' : obj.webhook };
  }

  return { kind: 'move', arg: '' };
}

// ── Human-readable summaries for the list view ───────────────────────

/**
 * One-line description of a stored match_expr JSON string.
 * Used in the rules list pane — must never throw.
 */
export function matchSummary(raw: string): string {
  let expr: unknown;
  try {
    expr = JSON.parse(raw);
  } catch {
    return '(invalid expression)';
  }
  return describeExpr(expr, 0);
}

function describeExpr(expr: unknown, depth: number): string {
  if (expr === null || expr === undefined) return '(empty)';
  if (typeof expr === 'string') return expr;

  const obj = expr as Record<string, unknown>;

  if (typeof obj.from === 'string') return `from ${obj.from}`;
  if (typeof obj.to === 'string') return `to ${obj.to}`;
  if (typeof obj.subject === 'string') return `subject matches "${obj.subject}"`;
  if (typeof obj.has_tag === 'string') return `tagged "${obj.has_tag}"`;
  if (typeof obj.contact_has_tag === 'string') return `contact tag "${obj.contact_has_tag}"`;

  if (obj.score_above && typeof obj.score_above === 'object') {
    const sa = obj.score_above as { dimension?: string; threshold?: number };
    return `${sa.dimension ?? '?'} score > ${sa.threshold ?? '?'}`;
  }
  if (obj.score_below && typeof obj.score_below === 'object') {
    const sb = obj.score_below as { dimension?: string; threshold?: number };
    return `${sb.dimension ?? '?'} score < ${sb.threshold ?? '?'}`;
  }

  if (Array.isArray(obj.and)) {
    if (depth > 1) return `(${obj.and.length} conditions)`;
    return obj.and.map((c) => describeExpr(c, depth + 1)).join(' AND ');
  }
  if (Array.isArray(obj.or)) {
    if (depth > 1) return `(${obj.or.length} conditions)`;
    return obj.or.map((c) => describeExpr(c, depth + 1)).join(' OR ');
  }
  if (obj.not !== undefined) return `NOT ${describeExpr(obj.not, depth + 1)}`;

  return '(complex expression)';
}

/**
 * One-line description of a stored action JSON string.
 * Webhook URLs are never shown here — the server already redacts them.
 */
export function actionSummary(raw: string): string {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return '(invalid action)';
  }

  if (parsed === 'delete') return 'Delete message';
  if (parsed === 'unsubscribe') return 'Unsubscribe + move to Junk';

  if (parsed && typeof parsed === 'object') {
    const obj = parsed as Record<string, unknown>;
    if (typeof obj.move === 'string') return `Move to ${obj.move}`;
    if (typeof obj.flag === 'string') return `Flag as ${obj.flag}`;
    if (typeof obj.unflag === 'string') return `Remove flag ${obj.unflag}`;
    if (typeof obj.add_tag === 'string') return `Tag as "${obj.add_tag}"`;
    if (typeof obj.snooze === 'string') return `Snooze (${obj.snooze})`;
    if (typeof obj.webhook === 'string') return 'Send webhook';
    if (typeof obj.reject === 'string') return `Reject: ${obj.reject}`;
    if (typeof obj.ereject === 'string') return `Reject (ESMTP): ${obj.ereject}`;
  }

  return '(unknown action)';
}

// ── Actions that carry safety risk ───────────────────────────────────

/**
 * Returns true for actions that destroy or remove messages from the mailbox.
 * The UI shows a confirm prompt before enabling a rule with these actions.
 */
export function isHighRiskAction(actionRaw: string): boolean {
  let parsed: unknown;
  try {
    parsed = JSON.parse(actionRaw);
  } catch {
    return false;
  }
  if (parsed === 'delete') return true;
  if (parsed === 'unsubscribe') return true;
  return false;
}

// ── API endpoints ─────────────────────────────────────────────────────

export const rulesApi = {
  list(accountId: string, o?: RequestOptions): Promise<RulesListResponse> {
    return request(`/accounts/${encodeURIComponent(accountId)}/rules`, o);
  },

  create(
    accountId: string,
    body: {
      name: string;
      match_expr: unknown;
      action: unknown;
      priority?: number;
      stop?: boolean;
      enabled?: boolean;
    },
    o?: RequestOptions
  ): Promise<RuleResponse> {
    return request(`/accounts/${encodeURIComponent(accountId)}/rules`, {
      ...o,
      method: 'POST',
      body
    });
  },

  update(
    accountId: string,
    ruleId: string,
    body: {
      name: string;
      match_expr: unknown;
      action: unknown;
      priority?: number;
      stop?: boolean;
    },
    o?: RequestOptions
  ): Promise<RuleResponse> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/rules/${encodeURIComponent(ruleId)}`,
      { ...o, method: 'PUT', body }
    );
  },

  destroy(accountId: string, ruleId: string, o?: RequestOptions): Promise<{ deleted: string }> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/rules/${encodeURIComponent(ruleId)}`,
      { ...o, method: 'DELETE' }
    );
  },

  enable(accountId: string, ruleId: string, o?: RequestOptions): Promise<{ enabled: true; rule_id: string }> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/rules/${encodeURIComponent(ruleId)}/enable`,
      { ...o, method: 'POST' }
    );
  },

  disable(accountId: string, ruleId: string, o?: RequestOptions): Promise<{ enabled: false; rule_id: string }> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/rules/${encodeURIComponent(ruleId)}/disable`,
      { ...o, method: 'POST' }
    );
  },

  preview(
    accountId: string,
    ruleId: string,
    body: { folder?: string; limit?: number },
    o?: RequestOptions
  ): Promise<unknown> {
    return request(
      `/accounts/${encodeURIComponent(accountId)}/rules/${encodeURIComponent(ruleId)}/preview`,
      { ...o, method: 'POST', body }
    );
  }
};
