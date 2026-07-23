import { describe, it, expect } from 'vitest';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { VaultIndex } from '../../src/vault/index';
import { searchNotes } from '../../src/vault/search';

async function vault(notes: { id: string; content: string }[]): Promise<VaultIndex> {
  const dir = await mkdtemp(join(tmpdir(), 'mv-sem-'));
  for (const n of notes) {
    const p = join(dir, `${n.id}.md`);
    await mkdir(dirname(p), { recursive: true });
    await writeFile(p, n.content, 'utf8');
  }
  return VaultIndex.build(dir);
}

describe('semantic / hybrid search (M10)', () => {
  it('hybrid surfaces a morphological variant that lexical search misses', async () => {
    const index = await vault([
      { id: 'a', content: 'Database migration strategy and rollout.' },
      { id: 'b', content: 'How to migrate the schema safely.' },
      { id: 'c', content: 'Gardening tips for spring.' },
    ]);
    const lexical = await searchNotes(index, 'migration', { mode: 'lexical' });
    const hybrid = await searchNotes(index, 'migration', { mode: 'hybrid' });

    expect(lexical.hits.map((h) => h.note_id)).not.toContain('b');
    expect(hybrid.hits.map((h) => h.note_id)).toContain('b');
    expect(hybrid.hits.map((h) => h.note_id)).not.toContain('c');
  });

  it('semantic mode ranks by similarity and respects the token budget', async () => {
    const index = await vault([
      { id: 'a', content: 'kubernetes cluster autoscaling and pods' },
      { id: 'b', content: 'unrelated poetry about the calm sea' },
    ]);
    const res = await searchNotes(index, 'kubernetes scaling', { mode: 'semantic', token_budget: 50 });
    expect(res.hits[0]?.note_id).toBe('a');
    expect(res.hits.reduce((s, h) => s + h.token_count, 0)).toBeLessThanOrEqual(50);
  });

  it('defaults to hybrid mode', async () => {
    const index = await vault([{ id: 'a', content: 'launch deadline in March' }]);
    const def = await searchNotes(index, 'launch');
    const hybrid = await searchNotes(index, 'launch', { mode: 'hybrid' });
    expect(def.hits.map((h) => h.note_id)).toEqual(hybrid.hits.map((h) => h.note_id));
  });
});
