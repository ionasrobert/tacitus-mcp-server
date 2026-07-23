import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdtemp, rm, access } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { HashingEmbedder, type Embedder } from '../../src/vault/embed';
import { CachedEmbedder } from '../../src/vault/embed-cache';

describe('CachedEmbedder (M11: vector persistence)', () => {
  let dir: string;
  let cacheFile: string;
  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), 'mv-vec-'));
    cacheFile = join(dir, '.tacitus', 'vectors', 'hash.json');
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('matches the inner embedder and writes a cache file', async () => {
    const inner = new HashingEmbedder();
    const cached = new CachedEmbedder(inner, cacheFile);
    const [v] = await cached.embedBatch(['hello world']);
    expect(v).toEqual((await inner.embedBatch(['hello world']))[0]);
    await expect(access(cacheFile)).resolves.toBeUndefined();
  });

  it('reuses persisted vectors across instances (survives restart)', async () => {
    const [v1] = await new CachedEmbedder(new HashingEmbedder(), cacheFile).embedBatch(['persist me']);
    const [v2] = await new CachedEmbedder(new HashingEmbedder(), cacheFile).embedBatch(['persist me']);
    expect(v2).toEqual(v1);
  });

  it('only calls the inner embedder for cache misses', async () => {
    let embedded = 0;
    const spy: Embedder = {
      dim: 4,
      embedBatch: async (texts) => {
        embedded += texts.length;
        return texts.map(() => [1, 0, 0, 0]);
      },
    };
    const cached = new CachedEmbedder(spy, cacheFile);
    await cached.embedBatch(['a', 'b']);
    await cached.embedBatch(['a', 'b', 'c']); // only 'c' is a miss
    expect(embedded).toBe(3);
  });
});
