import { describe, it, expect, beforeEach } from 'vitest';
import { VaultIndex } from '../../src/vault/index';
import { vaultTools } from '../../src/tools/vault';
import { runTool, type Tool } from '../../src/tools/types';
import { makeVault } from '../helpers/vault';

describe('vault tool contract (M6)', () => {
  let byName: Record<string, Tool>;

  beforeEach(async () => {
    const index = await VaultIndex.build(await makeVault());
    byName = Object.fromEntries(vaultTools(index).map((t) => [t.name, t]));
  });

  it('exposes the expected vault tools', () => {
    expect(Object.keys(byName).sort()).toEqual(['get_note', 'graph_query', 'list_notes', 'search']);
  });

  it('search: returns ranked hits', async () => {
    const res = await runTool(byName.search!, { query: 'launch deadline' });
    expect(res.ok).toBe(true);
    if (res.ok) expect((res.data as { hits: unknown[] }).hits.length).toBeGreaterThan(0);
  });

  it('get_note: unknown id returns a structured NOTE_NOT_FOUND', async () => {
    const res = await runTool(byName.get_note!, { note_id: 'missing', format: 'full' });
    expect(res.ok).toBe(false);
    if (!res.ok) expect(res.error.code).toBe('NOTE_NOT_FOUND');
  });

  it('graph_query: backlinks are structured', async () => {
    const res = await runTool(byName.graph_query!, {
      from: 'projects/launch',
      relation: 'backlinks',
    });
    expect(res.ok).toBe(true);
    if (res.ok) expect((res.data as { nodes: unknown[] }).nodes.length).toBe(2);
  });

  it('list_notes: returns every note', async () => {
    const res = await runTool(byName.list_notes!, {});
    expect(res.ok).toBe(true);
    if (res.ok) expect((res.data as { notes: unknown[] }).notes.length).toBe(3);
  });
});
