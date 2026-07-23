import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdtemp, rm, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { NoteWriter } from '../../src/vault/write';
import { writeTools } from '../../src/tools/write';
import { runTool, type Tool } from '../../src/tools/types';

describe('write tool contract (M7)', () => {
  let dir: string;
  let byName: Record<string, Tool>;

  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), 'mv-wc-'));
    byName = Object.fromEntries(writeTools(new NoteWriter(dir)).map((t) => [t.name, t]));
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('exposes the expected write tools', () => {
    expect(Object.keys(byName).sort()).toEqual(['commit_changes', 'propose_changes', 'revert']);
  });

  it('propose → commit round-trip writes the note', async () => {
    const proposed = await runTool(byName.propose_changes!, {
      ops: [{ op: 'create', note_id: 'n', content: 'hi' }],
    });
    expect(proposed.ok).toBe(true);
    const change_id = proposed.ok ? (proposed.data as { change_id: string }).change_id : '';
    const committed = await runTool(byName.commit_changes!, { change_id });
    expect(committed.ok).toBe(true);
    expect(await readFile(join(dir, 'n.md'), 'utf8')).toContain('hi');
  });

  it('commit with unknown change_id → structured UNKNOWN_CHANGE', async () => {
    const res = await runTool(byName.commit_changes!, { change_id: 'chg_nope' });
    expect(res.ok).toBe(false);
    if (!res.ok) expect(res.error.code).toBe('UNKNOWN_CHANGE');
  });

  it('malformed changeset → INVALID_INPUT', async () => {
    const res = await runTool(byName.propose_changes!, {
      ops: [{ op: 'frobnicate', note_id: 'x' }],
    });
    expect(res.ok).toBe(false);
    if (!res.ok) expect(res.error.code).toBe('INVALID_INPUT');
  });
});
