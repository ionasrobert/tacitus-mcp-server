import { describe, it, expect } from 'vitest';
import { recall } from '../../src/memory/recall';
import { makeMemory } from '../helpers/fixtures';

describe('recall (M2: relevance + token budget)', () => {
  const memories = [
    makeMemory({ id: 'mem_a', content: 'alpha beta gamma delta epsilon' }),
    makeMemory({ id: 'mem_b', content: 'beta beta beta' }),
    makeMemory({ id: 'mem_c', content: 'nothing relevant here at all' }),
  ];

  it('only returns relevant memories, ordered by descending score', () => {
    const res = recall(memories, { query: 'beta' });
    const scores = res.items.map((i) => i.score);
    expect(scores).toEqual([...scores].sort((a, b) => b - a));
    expect(res.items.every((i) => i.score > 0)).toBe(true);
    expect(res.items[0]?.memory.id).toBe('mem_b'); // strongest match first
    expect(res.items.map((i) => i.memory.id)).not.toContain('mem_c');
  });

  it('never exceeds the token budget', () => {
    const res = recall(memories, { query: 'beta alpha', token_budget: 5 });
    const total = res.items.reduce((s, i) => s + i.token_count, 0);
    expect(total).toBeLessThanOrEqual(5);
  });

  it('returns fewer items under a tighter budget', () => {
    const big = recall(memories, { query: 'beta alpha', token_budget: 1000 });
    const small = recall(memories, { query: 'beta alpha', token_budget: 3 });
    expect(small.items.length).toBeLessThan(big.items.length);
  });

  it('reports a positive token_count for every returned item', () => {
    const res = recall(memories, { query: 'beta' });
    expect(res.items.length).toBeGreaterThan(0);
    for (const item of res.items) {
      expect(item.token_count).toBeGreaterThan(0);
    }
  });

  it('filters by memory type when requested', () => {
    const mixed = [
      makeMemory({ id: 'u', type: 'user', content: 'preference alpha' }),
      makeMemory({ id: 'p', type: 'project', content: 'preference alpha' }),
    ];
    const res = recall(mixed, { query: 'preference', type: 'project' });
    expect(res.items.map((i) => i.memory.id)).toEqual(['p']);
  });
});
