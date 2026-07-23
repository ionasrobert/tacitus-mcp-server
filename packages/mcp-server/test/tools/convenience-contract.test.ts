import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { NoteWriter } from '../../src/vault/write';
import { convenienceTools } from '../../src/tools/convenience';
import { runTool, type Tool } from '../../src/tools/types';

describe('convenience tool contract (M9)', () => {
  let dir: string;
  let byName: Record<string, Tool>;

  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), 'mv-cc-'));
    byName = Object.fromEntries(convenienceTools(new NoteWriter(dir)).map((t) => [t.name, t]));
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('exposes the expected tools', () => {
    expect(Object.keys(byName).sort()).toEqual([
      'audit_log',
      'create_note',
      'link',
      'tag',
      'update_note',
    ]);
  });

  it('create_note then audit_log shows the action', async () => {
    const created = await runTool(byName.create_note!, { note_id: 'n', content: 'hi' });
    expect(created.ok).toBe(true);
    const audit = await runTool(byName.audit_log!, {});
    expect(audit.ok).toBe(true);
    if (audit.ok) expect((audit.data as { entries: unknown[] }).entries.length).toBeGreaterThan(0);
  });

  it('create_note with missing content → INVALID_INPUT', async () => {
    const res = await runTool(byName.create_note!, { note_id: 'n' });
    expect(res.ok).toBe(false);
    if (!res.ok) expect(res.error.code).toBe('INVALID_INPUT');
  });
});
