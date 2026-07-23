import { estimate } from '../lib/tokens';
import { detectConflicts, type Conflict } from './conflict';
import { LexicalRanker, type Ranker } from './rank';
import type { Memory, MemoryType } from './types';

export interface RecallArgs {
  query: string;
  type?: MemoryType;
  /** Hard ceiling: the sum of returned token_count never exceeds this. */
  token_budget?: number;
}

export interface RecallItem {
  memory: Memory;
  score: number;
  token_count: number;
}

export interface RecallResult {
  items: RecallItem[];
  conflicts: Conflict[];
}

const DEFAULT_RANKER = new LexicalRanker();

/**
 * Rank relevant memories and return as many as fit under token_budget.
 * Budget is a hard ceiling (Part I.1): an item that doesn't fit is skipped, but
 * we keep trying smaller ones. Conflicts are wired in M3.
 */
export function recall(
  memories: Memory[],
  args: RecallArgs,
  ranker: Ranker = DEFAULT_RANKER,
): RecallResult {
  const candidates = args.type ? memories.filter((m) => m.type === args.type) : memories;
  const ranked = ranker.rank(args.query, candidates).filter((s) => s.score > 0);

  const budget = args.token_budget ?? Number.POSITIVE_INFINITY;
  const items: RecallItem[] = [];
  let used = 0;
  for (const { memory, score } of ranked) {
    const token_count = estimate(memory.content);
    if (used + token_count > budget) continue;
    items.push({ memory, score, token_count });
    used += token_count;
  }

  // Compute conflicts over the relevant set — not the budget-truncated items —
  // so truncation can never hide a contradiction.
  return { items, conflicts: detectConflicts(ranked.map((s) => s.memory)) };
}
