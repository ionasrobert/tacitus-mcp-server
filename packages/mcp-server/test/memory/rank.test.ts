import { describe, it, expect } from 'vitest';
import { LexicalRanker } from '../../src/memory/rank';
import { makeMemory } from '../helpers/fixtures';

describe('LexicalRanker (M2: relevance)', () => {
  const ranker = new LexicalRanker();

  it('scores higher for more query-term frequency', () => {
    const m1 = makeMemory({ id: 'm1', content: 'the quick brown fox' });
    const m2 = makeMemory({ id: 'm2', content: 'quick fox quick fox' });
    const scored = ranker.rank('quick fox', [m1, m2]);
    const byId = Object.fromEntries(scored.map((s) => [s.memory.id, s.score]));
    expect(byId.m2).toBeGreaterThan(byId.m1!);
  });

  it('gives a zero score to non-matching memories', () => {
    const m = makeMemory({ id: 'm', content: 'completely unrelated text' });
    const scored = ranker.rank('quantum', [m]);
    expect(scored[0]?.score).toBe(0);
  });

  it('returns results sorted by descending score', () => {
    const mems = [
      makeMemory({ id: 'a', content: 'apple' }),
      makeMemory({ id: 'b', content: 'apple apple apple' }),
      makeMemory({ id: 'c', content: 'banana' }),
    ];
    const scores = ranker.rank('apple', mems).map((s) => s.score);
    expect(scores).toEqual([...scores].sort((x, y) => y - x));
  });

  it('is case-insensitive', () => {
    const m = makeMemory({ id: 'm', content: 'Distributed Systems' });
    const scored = ranker.rank('distributed', [m]);
    expect(scored[0]?.score).toBeGreaterThan(0);
  });
});
