import { describe, it, expect } from 'vitest';
import { cosine } from '../../src/vault/embed';
import { TransformersEmbedder } from '../../src/vault/transformers';

// Neural integration test. Runs ONLY when the optional dependency
// @huggingface/transformers is installed (i.e. not in CI) and downloads a
// model on first run. It proves the neural embedder captures synonyms —
// something the deterministic HashingEmbedder cannot.
let available = false;
try {
  const spec = '@huggingface/transformers';
  await import(spec);
  available = true;
} catch {
  available = false;
}

describe.skipIf(!available)('TransformersEmbedder (M11 integration — neural)', () => {
  it(
    'scores synonyms far higher than unrelated text',
    async () => {
      const embedder = new TransformersEmbedder();
      const [car, automobile, gardening] = await embedder.embedBatch([
        'car',
        'automobile',
        'gardening tips for spring',
      ]);
      const synonym = cosine(car!, automobile!);
      const unrelated = cosine(car!, gardening!);
      expect(synonym).toBeGreaterThan(0.5);
      expect(synonym).toBeGreaterThan(unrelated + 0.3);
    },
    120_000,
  );
});
