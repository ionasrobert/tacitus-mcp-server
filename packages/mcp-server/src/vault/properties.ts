import { estimate } from '../lib/tokens';
import type { VaultIndex } from './index';

/**
 * Bases-like structured queries over typed frontmatter (Part I.3 / III):
 * `properties_query` treats the vault's YAML properties as a queryable
 * database — "all notes where status=active and due < 2026-08-01" is a query,
 * not a fuzzy search. Output is bounded (limit + token_budget) and every row
 * carries its own token cost. Port of crates/tacitus-core/src/vault/properties.rs.
 */

export const PROP_OPS = [
  'eq',
  'ne',
  'contains',
  'exists',
  'not_exists',
  'gt',
  'lt',
  'gte',
  'lte',
] as const;
export type PropOp = (typeof PROP_OPS)[number];

/** A single filter condition. All of a query's filters are AND-ed. */
export interface PropFilter {
  key: string;
  op: PropOp;
  /** Comparison value; unused for exists / not_exists. */
  value?: unknown;
}

export interface PropertiesQueryArgs {
  filters?: PropFilter[];
  /** Property keys to include in each row (default: all). */
  select?: string[];
  /** Sort by this property's value (missing values sort last). */
  sort_by?: string;
  descending?: boolean;
  /** Max rows (default 50). */
  limit?: number;
  /** Hard token ceiling across all returned rows. */
  token_budget?: number;
}

export interface PropertiesRow {
  note_id: string;
  title: string;
  properties: Record<string, unknown>;
  token_count: number;
}

const DEFAULT_LIMIT = 50;

function jsonEq(a: unknown, b: unknown): boolean {
  if (typeof a === 'number' && typeof b === 'number') return a === b;
  return JSON.stringify(a) === JSON.stringify(b);
}

/** Ordered comparison: numeric for two numbers, lexicographic for two strings, else null. */
function jsonCmp(a: unknown, b: unknown): number | null {
  if (typeof a === 'number' && typeof b === 'number') {
    if (Number.isNaN(a) || Number.isNaN(b)) return null;
    return a < b ? -1 : a > b ? 1 : 0;
  }
  if (typeof a === 'string' && typeof b === 'string') {
    // Code-point order, like Rust's str::cmp (not locale collation).
    return a < b ? -1 : a > b ? 1 : 0;
  }
  return null;
}

function matches(props: Record<string, unknown>, filter: PropFilter): boolean {
  const has = Object.prototype.hasOwnProperty.call(props, filter.key);
  if (filter.op === 'exists') return has;
  if (filter.op === 'not_exists') return !has;
  const actual = props[filter.key];
  const expected = filter.value;
  // A missing key or a missing/null comparison value never matches.
  if (!has || expected === undefined || expected === null) return false;
  switch (filter.op) {
    case 'eq':
      return jsonEq(actual, expected);
    case 'ne':
      return !jsonEq(actual, expected);
    case 'contains':
      if (Array.isArray(actual)) return actual.some((item) => jsonEq(item, expected));
      if (typeof actual === 'string' && typeof expected === 'string') {
        return actual.toLowerCase().includes(expected.toLowerCase());
      }
      return false;
    case 'gt':
      return jsonCmp(actual, expected) === 1;
    case 'lt':
      return jsonCmp(actual, expected) === -1;
    case 'gte': {
      const c = jsonCmp(actual, expected);
      return c === 1 || c === 0;
    }
    case 'lte': {
      const c = jsonCmp(actual, expected);
      return c === -1 || c === 0;
    }
  }
}

/**
 * Query notes by their typed frontmatter properties. Deterministic: rows are
 * ordered by `sort_by` (or note_id), then capped by `limit` and `token_budget`.
 */
export function propertiesQuery(
  index: VaultIndex,
  args: PropertiesQueryArgs = {},
): PropertiesRow[] {
  const filters = args.filters ?? [];
  let rows = index
    .all()
    .filter((note) => filters.every((f) => matches(note.frontmatter, f)))
    .map((note) => ({ note_id: note.id, title: note.title, props: note.frontmatter }))
    .sort((a, b) => (a.note_id < b.note_id ? -1 : a.note_id > b.note_id ? 1 : 0));

  const sortKey = args.sort_by;
  if (sortKey !== undefined) {
    // Array.prototype.sort is stable, like Rust's sort_by — ties keep note_id order.
    rows.sort((a, b) => {
      const hasA = Object.prototype.hasOwnProperty.call(a.props, sortKey);
      const hasB = Object.prototype.hasOwnProperty.call(b.props, sortKey);
      let cmp: number;
      if (hasA && hasB) cmp = jsonCmp(a.props[sortKey], b.props[sortKey]) ?? 0;
      else if (hasA)
        cmp = -1; // missing sorts last
      else if (hasB) cmp = 1;
      else cmp = 0;
      return args.descending ? -cmp : cmp;
    });
  }

  rows = rows.slice(0, args.limit ?? DEFAULT_LIMIT);

  const budget = args.token_budget ?? Number.POSITIVE_INFINITY;
  let used = 0;
  const out: PropertiesRow[] = [];
  for (const { note_id, title, props } of rows) {
    const keys = Object.keys(props)
      .sort()
      .filter((k) => args.select === undefined || args.select.includes(k));
    const properties: Record<string, unknown> = {};
    for (const k of keys) properties[k] = props[k];
    const token_count = estimate(JSON.stringify({ note_id, properties, title }));
    if (used + token_count > budget) continue;
    used += token_count;
    out.push({ note_id, title, properties, token_count });
  }
  return out;
}
