import { describe, it, expect, beforeEach } from 'vitest';
import { VaultIndex } from '../../src/vault/index';
import { getNote } from '../../src/vault/get';
import { TacitusError } from '../../src/lib/errors';
import { makeVault } from '../helpers/vault';

describe('getNote (M6: progressive disclosure)', () => {
  let index: VaultIndex;
  beforeEach(async () => {
    index = await VaultIndex.build(await makeVault());
  });

  it('returns an outline (headings only)', () => {
    const res = getNote(index, 'projects/launch', { format: 'outline' });
    expect(res.content).toContain('Timeline');
    expect(res.content).toContain('Risks');
    expect(res.content).not.toContain('[[ideas]]'); // body not included
  });

  it('returns frontmatter only', () => {
    const res = getNote(index, 'projects/launch', { format: 'frontmatter_only' });
    expect(res.content).toContain('title');
    expect(res.content).not.toContain('Mitigations');
  });

  it('returns full content, truncated to max_tokens', () => {
    const res = getNote(index, 'projects/launch', { format: 'full', max_tokens: 3 });
    expect(res.truncated).toBe(true);
    expect(res.token_count).toBeLessThanOrEqual(3);
  });

  it('returns full content untruncated when it fits', () => {
    const res = getNote(index, 'projects/launch', { format: 'full', max_tokens: 10000 });
    expect(res.truncated).toBe(false);
    expect(res.content).toContain('Launch overview');
  });

  it('throws NOTE_NOT_FOUND for a missing note', () => {
    try {
      getNote(index, 'nope', { format: 'full' });
      throw new Error('expected getNote to throw');
    } catch (err) {
      expect(err).toBeInstanceOf(TacitusError);
      expect((err as TacitusError).code).toBe('NOTE_NOT_FOUND');
    }
  });
});
