import { describe, it, expect, beforeEach } from 'vitest';
import { VaultIndex } from '../../src/vault/index';
import { graphQuery } from '../../src/vault/graph';
import { TacitusError } from '../../src/lib/errors';
import { makeVault } from '../helpers/vault';

describe('graphQuery (M6: link graph as API)', () => {
  let index: VaultIndex;
  beforeEach(async () => {
    index = await VaultIndex.build(await makeVault());
  });

  it('returns outgoing links', () => {
    const res = graphQuery(index, { from: 'ideas', relation: 'links' });
    expect(res.nodes.map((n) => n.note_id)).toContain('projects/launch');
  });

  it('returns backlinks', () => {
    const res = graphQuery(index, { from: 'projects/launch', relation: 'backlinks' });
    expect(res.nodes.map((n) => n.note_id).sort()).toEqual(['ideas', 'index']);
  });

  it('returns neighbors in both directions', () => {
    const res = graphQuery(index, { from: 'projects/launch', relation: 'neighbors' });
    const ids = res.nodes.map((n) => n.note_id);
    expect(ids).toContain('ideas');
    expect(ids).toContain('index');
  });

  it('throws NOTE_NOT_FOUND for a missing node', () => {
    try {
      graphQuery(index, { from: 'nope', relation: 'links' });
      throw new Error('expected graphQuery to throw');
    } catch (err) {
      expect((err as TacitusError).code).toBe('NOTE_NOT_FOUND');
    }
  });
});
