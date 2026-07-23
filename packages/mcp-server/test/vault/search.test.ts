import { describe, it, expect, beforeEach } from 'vitest';
import { VaultIndex } from '../../src/vault/index';
import { searchNotes } from '../../src/vault/search';
import { makeVault } from '../helpers/vault';

describe('searchNotes (M6: retrieval + token budget)', () => {
  let index: VaultIndex;
  beforeEach(async () => {
    index = await VaultIndex.build(await makeVault());
  });

  it('finds notes by content relevance, ranked descending', async () => {
    const res = await searchNotes(index, 'launch deadline');
    expect(res.hits.length).toBeGreaterThan(0);
    const scores = res.hits.map((h) => h.score);
    expect(scores).toEqual([...scores].sort((a, b) => b - a));
    expect(res.hits.map((h) => h.note_id)).toContain('ideas');
  });

  it('never exceeds the token budget', async () => {
    const res = await searchNotes(index, 'launch', { token_budget: 8 });
    const total = res.hits.reduce((s, h) => s + h.token_count, 0);
    expect(total).toBeLessThanOrEqual(8);
  });

  it('returns snippets, not whole notes', async () => {
    const res = await searchNotes(index, 'launch');
    expect(res.hits.length).toBeGreaterThan(0);
    for (const h of res.hits) expect(h.snippet.length).toBeLessThanOrEqual(240);
  });

  it('respects top_k', async () => {
    const res = await searchNotes(index, 'launch', { top_k: 1 });
    expect(res.hits.length).toBeLessThanOrEqual(1);
  });
});
