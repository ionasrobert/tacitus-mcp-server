/**
 * Embeddings for semantic search (Part I.1/I.3).
 *
 * `Embedder.embedBatch` is async so a real neural model (transformers.js / an
 * embeddings API) fits the same interface. The default HashingEmbedder is
 * deterministic and offline — it hashes word tokens AND character trigrams into
 * a fixed-dimension vector, so morphological variants ("migrate" ~ "migration")
 * land near each other, which pure lexical matching misses. A neural embedder
 * additionally captures synonyms/paraphrases; it drops in without touching search.
 */
export interface Embedder {
  readonly dim: number;
  embedBatch(texts: string[]): Promise<number[][]>;
}

/** Cosine similarity of two unit-normalized vectors (returns 0 for a zero vector). */
export function cosine(a: number[], b: number[]): number {
  let dot = 0;
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i += 1) dot += (a[i] ?? 0) * (b[i] ?? 0);
  return dot;
}

function fnv1a(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i += 1) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

function features(text: string): string[] {
  const tokens = text.toLowerCase().match(/[a-z0-9]+/g) ?? [];
  const feats: string[] = [];
  for (const token of tokens) {
    feats.push(`w:${token}`);
    const padded = `#${token}#`;
    for (let i = 0; i + 3 <= padded.length; i += 1) feats.push(`t:${padded.slice(i, i + 3)}`);
  }
  return feats;
}

export class HashingEmbedder implements Embedder {
  private readonly cache = new Map<string, number[]>();

  // 2048 dims keep hash collisions low enough that unrelated texts score near
  // zero while morphological variants stay well separated (see eval scenarios).
  constructor(readonly dim = 2048) {}

  /** Synchronous single-text embedding (used directly by unit tests). */
  embed(text: string): number[] {
    const cached = this.cache.get(text);
    if (cached) return cached;

    const vec = new Array<number>(this.dim).fill(0);
    for (const feature of features(text)) {
      const idx = fnv1a(feature) % this.dim;
      vec[idx] = (vec[idx] ?? 0) + 1;
    }

    let norm = 0;
    for (const x of vec) norm += x * x;
    norm = Math.sqrt(norm);
    const normalized = norm === 0 ? vec : vec.map((x) => x / norm);

    if (this.cache.size > 4096) this.cache.clear();
    this.cache.set(text, normalized);
    return normalized;
  }

  async embedBatch(texts: string[]): Promise<number[][]> {
    return texts.map((text) => this.embed(text));
  }
}
