import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { access, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { structuredReadTools, structuredWriteTools } from '../../src/tools/structured';
import { runTool, type Tool } from '../../src/tools/types';
import { VaultIndex } from '../../src/vault/index';
import { NoteWriter } from '../../src/vault/write';
import { makeVault } from '../helpers/vault';

async function exists(p: string): Promise<boolean> {
  try {
    await access(p);
    return true;
  } catch {
    return false;
  }
}

describe('structured tool contract (ported from the native engine)', () => {
  let dir: string;
  let t: Record<string, Tool>;

  async function call<T = Record<string, unknown>>(name: string, args: unknown): Promise<T> {
    const res = await runTool(t[name]!, args);
    if (!res.ok) throw new Error(`${name} failed: ${JSON.stringify(res.error)}`);
    return res.data as T;
  }
  async function errorCode(name: string, args: unknown): Promise<string> {
    const res = await runTool(t[name]!, args);
    expect(res.ok).toBe(false);
    return res.ok ? '' : res.error.code;
  }

  beforeEach(async () => {
    dir = await makeVault();
    await mkdir(join(dir, '.tacitus', 'templates'), { recursive: true });
    await writeFile(
      join(dir, '.tacitus', 'templates', 'meeting.md'),
      '---\ntitle: "{{title}}"\npriority: {{p}}\n---\n# {{title}}\n\n- [ ] follow up due:2026-12-01\n',
    );
    const index = await VaultIndex.build(dir);
    const writer = new NoteWriter(dir, 'read-write', index);
    t = Object.fromEntries(
      [...structuredReadTools(index, dir), ...structuredWriteTools(index, dir, writer)].map((x) => [
        x.name,
        x,
      ]),
    );
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('exposes the nine ported tools', () => {
    expect(Object.keys(t).sort()).toEqual([
      'create_from_template',
      'delete_note',
      'get_version',
      'list_tasks',
      'list_templates',
      'properties_query',
      'rename_note',
      'suggest_links',
      'toggle_task',
    ]);
  });

  it('templates → typed note → task toggle → version inspection', async () => {
    expect(await call('list_templates', {})).toEqual({
      templates: [{ name: 'meeting', vars: ['title', 'p'] }],
    });
    expect(
      await errorCode('create_from_template', {
        template: 'meeting',
        note_id: 'm/1',
        vars: { title: 'x' },
      }),
    ).toBe('MISSING_VARS');
    expect(
      await errorCode('create_from_template', {
        template: 'meeting',
        note_id: 'm/1',
        vars: { title: 'x', p: [1] },
      }),
    ).toBe('INVALID_INPUT');

    const created = await call<{ version_id: string; note_id: string }>('create_from_template', {
      template: 'meeting',
      note_id: 'm/1',
      vars: { title: 'Standup', p: 3 },
    });
    expect(created.note_id).toBe('m/1');

    const { rows } = await call<{
      rows: { note_id: string; properties: Record<string, unknown> }[];
    }>('properties_query', { filters: [{ key: 'priority', op: 'eq', value: 3 }] });
    expect(rows).toEqual([
      expect.objectContaining({ note_id: 'm/1', properties: { priority: 3, title: 'Standup' } }),
    ]);

    const { tasks } = await call<{ tasks: { note_id: string; line: number; text: string }[] }>(
      'list_tasks',
      {
        note_id: 'm/1',
      },
    );
    expect(tasks).toHaveLength(1);
    const task = tasks[0]!;
    expect(await errorCode('toggle_task', { ...task, expect_text: 'stale' })).toBe('CONFLICT');
    const toggled = await call<{ version_id: string }>('toggle_task', {
      note_id: task.note_id,
      line: task.line,
      expect_text: task.text,
    });
    expect(await readFile(join(dir, 'm/1.md'), 'utf8')).toContain('- [x] follow up');

    const ops = await call<{
      notes: { note_id: string; op: string; before: unknown; after: unknown }[];
    }>('get_version', { version_id: created.version_id });
    expect(ops.notes).toEqual([{ note_id: 'm/1', op: 'created', before: null, after: null }]);

    const full = await call<{
      notes: { before: { content: string }; after: { content: string; truncated: boolean } }[];
    }>('get_version', { version_id: toggled.version_id, include_content: true, max_tokens: 5 });
    expect(full.notes[0]!.after).toEqual({ content: expect.any(String), truncated: true });
    expect(full.notes[0]!.after.content).toHaveLength(20);
    expect(await errorCode('get_version', { version_id: 'v_nope' })).toBe('UNKNOWN_VERSION');
  });

  it('toggle_task guards against edits made outside Tacitus', async () => {
    const { tasks } = await call<{ tasks: { note_id: string; line: number; text: string }[] }>(
      'list_tasks',
      {},
    );
    expect(tasks).toHaveLength(0);
    await writeFile(join(dir, 'ideas.md'), '- [ ] edited by hand\n');
    // The index has not seen the edit; the tool reads the disk and still guards.
    expect(
      await errorCode('toggle_task', { note_id: 'ideas', line: 0, expect_text: 'something else' }),
    ).toBe('CONFLICT');
    await call('toggle_task', { note_id: 'ideas', line: 0, expect_text: 'edited by hand' });
    expect(await readFile(join(dir, 'ideas.md'), 'utf8')).toBe('- [x] edited by hand\n');
  });

  it('rename_note reports links_updated_in and delete_note is revertible via get_version', async () => {
    const renamed = await call<{ links_updated_in: number; from: string; to: string }>(
      'rename_note',
      {
        from: 'ideas',
        to: 'brainstorm',
      },
    );
    expect(renamed).toMatchObject({ from: 'ideas', to: 'brainstorm', links_updated_in: 2 });
    expect(await readFile(join(dir, 'index.md'), 'utf8')).toContain('[[brainstorm]]');

    const del = await call<{ version_id: string }>('delete_note', { note_id: 'brainstorm' });
    expect(await exists(join(dir, 'brainstorm.md'))).toBe(false);
    const v = await call<{ notes: { op: string }[] }>('get_version', {
      version_id: del.version_id,
    });
    expect(v.notes).toEqual([expect.objectContaining({ note_id: 'brainstorm', op: 'deleted' })]);
    expect(await errorCode('delete_note', { note_id: 'brainstorm' })).toBe('NOTE_NOT_FOUND');
  });

  it('properties_query rejects an unknown op with the native message', async () => {
    const res = await runTool(t.properties_query!, {
      filters: [{ key: 'a', op: 'like', value: 'x' }],
    });
    expect(res).toMatchObject({
      ok: false,
      error: { code: 'INVALID_INPUT', reason: expect.stringContaining('(got "like")') },
    });
  });

  it('suggest_links wraps suggestions with the source id', async () => {
    const res = await call<{ note_id: string; suggestions: unknown[] }>('suggest_links', {
      note_id: 'ideas',
    });
    expect(res.note_id).toBe('ideas');
    expect(Array.isArray(res.suggestions)).toBe(true);
    expect(await errorCode('suggest_links', { note_id: 'nope' })).toBe('NOTE_NOT_FOUND');
  });
});
