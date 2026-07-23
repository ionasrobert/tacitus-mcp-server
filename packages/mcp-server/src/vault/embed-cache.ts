import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import { dirname } from 'node:path';
import { stableId } from '../lib/ids';
import type { Embedder } from './embed';

/**
 * Wraps an embedder with an on-disk vector cache (`.tacitus/vectors/*.json`),
 * so an expensive neural embedder never recomputes a vector for unchanged text —
 * across restarts. Keyed by content hash.
 */
export class CachedEmbedder implements Embedder {
  readonly dim: number;
  private store: Record<string, number[]> | null = null;

  constructor(
    private readonly inner: Embedder,
    private readonly cacheFile: string,
  ) {
    this.dim = inner.dim;
  }

  private async load(): Promise<Record<string, number[]>> {
    if (this.store) return this.store;
    try {
      this.store = JSON.parse(await readFile(this.cacheFile, 'utf8')) as Record<string, number[]>;
    } catch {
      this.store = {};
    }
    return this.store;
  }

  async embedBatch(texts: string[]): Promise<number[][]> {
    const store = await this.load();
    const missing = [...new Set(texts.filter((t) => !(keyOf(t) in store)))];
    if (missing.length > 0) {
      const vecs = await this.inner.embedBatch(missing);
      missing.forEach((text, i) => {
        store[keyOf(text)] = vecs[i] ?? [];
      });
      await this.save(store);
    }
    return texts.map((t) => store[keyOf(t)] ?? []);
  }

  private async save(store: Record<string, number[]>): Promise<void> {
    await mkdir(dirname(this.cacheFile), { recursive: true });
    const tmp = `${this.cacheFile}.tmp`;
    await writeFile(tmp, JSON.stringify(store), 'utf8');
    await rename(tmp, this.cacheFile);
  }
}

function keyOf(text: string): string {
  return stableId(text, 'v');
}
