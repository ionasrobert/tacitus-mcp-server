import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdtemp, rm, writeFile, mkdir, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { MemoryStore } from '../../src/memory/store';
import { makeMemory } from '../helpers/fixtures';

describe('MemoryStore (M4: persistence + idempotency)', () => {
  let dir: string;

  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), 'tacitus-'));
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('returns an empty array for a fresh vault', async () => {
    const store = new MemoryStore(dir);
    expect(await store.load()).toEqual([]);
  });

  it('round-trips a memory through save/load', async () => {
    const store = new MemoryStore(dir);
    const mem = makeMemory({
      id: 'mem_round',
      content: 'persisted content\nwith two lines',
      key: 'k1',
      tags: ['x', 'y'],
    });
    await store.save(mem);
    const loaded = await store.load();
    expect(loaded).toHaveLength(1);
    expect(loaded[0]).toEqual(mem);
  });

  it('is idempotent: saving the same id twice yields one file', async () => {
    const store = new MemoryStore(dir);
    const mem = makeMemory({ id: 'mem_dup', content: 'once' });
    await store.save(mem);
    await store.save(mem);
    const files = await readdir(join(dir, '.tacitus', 'memory'));
    expect(files).toHaveLength(1);
    expect(await store.load()).toHaveLength(1);
  });

  it('skips a corrupt file instead of crashing', async () => {
    const store = new MemoryStore(dir);
    await store.save(makeMemory({ id: 'mem_good', content: 'valid' }));
    const memDir = join(dir, '.tacitus', 'memory');
    await mkdir(memDir, { recursive: true });
    await writeFile(join(memDir, 'corrupt.md'), 'not valid frontmatter at all', 'utf8');
    const loaded = await store.load();
    expect(loaded.map((m) => m.id)).toEqual(['mem_good']);
  });

  it('removes a memory by id', async () => {
    const store = new MemoryStore(dir);
    await store.save(makeMemory({ id: 'mem_x', content: 'gone soon' }));
    expect(await store.remove('mem_x')).toBe(true);
    expect(await store.remove('mem_missing')).toBe(false);
    expect(await store.load()).toEqual([]);
  });
});
