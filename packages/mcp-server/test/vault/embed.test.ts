import { describe, it, expect } from 'vitest';
import { HashingEmbedder, cosine } from '../../src/vault/embed';

describe('HashingEmbedder (M10)', () => {
  const e = new HashingEmbedder(256);

  it('produces unit-normalized vectors of fixed dimension', () => {
    const v = e.embed('hello world');
    expect(v).toHaveLength(256);
    const norm = Math.sqrt(v.reduce((s, x) => s + x * x, 0));
    expect(norm).toBeCloseTo(1, 5);
  });

  it('is deterministic', () => {
    expect(e.embed('same text here')).toEqual(e.embed('same text here'));
  });

  it('cosine is 1 for identical text and low for unrelated text', () => {
    expect(cosine(e.embed('database migration'), e.embed('database migration'))).toBeCloseTo(1, 5);
    expect(cosine(e.embed('database migration'), e.embed('coffee brewing'))).toBeLessThan(0.2);
  });

  it('places morphological variants closer than unrelated text', () => {
    const q = e.embed('migration');
    const variant = cosine(q, e.embed('migrate the schema'));
    const unrelated = cosine(q, e.embed('coffee brewing methods'));
    expect(variant).toBeGreaterThan(unrelated);
  });

  it('handles empty text without NaN', () => {
    const v = e.embed('');
    expect(v.every((x) => Number.isFinite(x))).toBe(true);
    expect(cosine(v, e.embed('anything'))).toBe(0);
  });
});
