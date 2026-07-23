import { describe, it, expect } from 'vitest';
import { setScore, tokenEfficiency } from '../../src/eval/metrics';

describe('eval metrics (M8)', () => {
  it('computes precision, recall, and f1', () => {
    const s = setScore(['a', 'b'], ['a', 'b', 'c']);
    expect(s.precision).toBe(1);
    expect(s.recall).toBeCloseTo(2 / 3, 5);
    expect(s.f1).toBeCloseTo(0.8, 5);
  });

  it('penalises retrieving an irrelevant item (precision drops)', () => {
    const s = setScore(['a', 'x'], ['a']);
    expect(s.precision).toBe(0.5);
    expect(s.recall).toBe(1);
  });

  it('treats an empty relevant set as perfect recall', () => {
    expect(setScore([], []).recall).toBe(1);
  });

  it('token efficiency is the fraction of returned tokens that are relevant', () => {
    const items = [
      { id: 'a', token_count: 10 },
      { id: 'b', token_count: 30 },
    ];
    expect(tokenEfficiency(items, ['a'])).toBeCloseTo(0.25, 5);
    expect(tokenEfficiency([{ id: 'a', token_count: 10 }], ['a'])).toBe(1);
  });
});
