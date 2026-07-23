import { lexicalScore } from '../lib/lexical';
import type { Memory } from './types';

export interface Scored {
  memory: Memory;
  score: number;
}

/** Pluggable relevance ranking (Part I.1) — swap lexical for embeddings later. */
export interface Ranker {
  rank(query: string, memories: Memory[]): Scored[];
}

/**
 * Lexical ranker over memory content (case-insensitive summed term frequency).
 * Returns every memory (including score 0) sorted by descending score;
 * callers filter by score > 0 as needed.
 */
export class LexicalRanker implements Ranker {
  rank(query: string, memories: Memory[]): Scored[] {
    return memories
      .map((memory) => ({ memory, score: lexicalScore(query, memory.content) }))
      .sort((a, b) => b.score - a.score);
  }
}
