import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { access, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { typeAndCaptureTools } from '../../src/tools/capture';
import { convenienceTools } from '../../src/tools/convenience';
import { structuredReadTools, structuredWriteTools } from '../../src/tools/structured';
import { runTool, type PermissionScope, type Tool } from '../../src/tools/types';
import { writeTools } from '../../src/tools/write';
import { appendLine, captureLine, dailyNoteId } from '../../src/vault/daily';
import { VaultIndex } from '../../src/vault/index';
import { TypeStore, violations, parseTypeDef } from '../../src/vault/schema';
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

const IDEA =
  'description: Article idea\nproperties:\n  status: { type: string, required: true, enum: [idea, draft] }\n  priority: { type: number }\n';

describe('declared types + capture (mirrors crates/tacitus-core tools/tests.rs)', () => {
  let dir: string;
  let t: Record<string, Tool>;

  async function build(scope: PermissionScope = 'read-write'): Promise<void> {
    const index = await VaultIndex.build(dir);
    const writer = new NoteWriter(dir, scope, index);
    t = Object.fromEntries(
      [
        ...writeTools(writer),
        ...convenienceTools(writer),
        ...structuredReadTools(index, dir),
        ...structuredWriteTools(index, dir, writer),
        ...typeAndCaptureTools(dir, writer),
      ].map((tool) => [tool.name, tool]),
    );
  }
  async function call(name: string, args: unknown): Promise<Record<string, any>> {
    const res = await runTool(t[name]!, args);
    if (!res.ok) throw new Error(`${name} failed: ${JSON.stringify(res.error)}`);
    return res.data as Record<string, any>;
  }
  async function error(name: string, args: unknown): Promise<{ code: string; reason: string }> {
    const res = await runTool(t[name]!, args);
    expect(res.ok).toBe(false);
    return res.ok ? { code: '', reason: '' } : res.error;
  }

  beforeEach(async () => {
    dir = await makeVault();
    await build();
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('declared types gate writes, and list_types describes them', async () => {
    await mkdir(join(dir, '.tacitus', 'types'), { recursive: true });
    await writeFile(join(dir, '.tacitus', 'types', 'idea.yaml'), IDEA);

    const listed = await call('list_types', {});
    expect(listed.types[0]).toEqual({
      name: 'idea',
      description: 'Article idea',
      properties: {
        priority: { type: 'number', required: false },
        status: { type: 'string', required: true, enum: ['idea', 'draft'] },
      },
    });

    const bad = await error('create_note', {
      note_id: 'ideas/a',
      content: 'x',
      frontmatter: { type: 'idea', priority: 'high' },
    });
    expect(bad.code).toBe('SCHEMA_VIOLATION');
    expect(bad.reason).toContain('missing required property "status"');
    expect(bad.reason).toContain('"priority" must be number, got string "high"');
    expect(await exists(join(dir, 'ideas', 'a.md'))).toBe(false);

    await call('create_note', {
      note_id: 'ideas/a',
      content: 'x',
      frontmatter: { type: 'idea', status: 'idea', priority: 2 },
    });
    expect(
      (
        await error('update_note', {
          note_id: 'ideas/a',
          frontmatter: { type: 'idea', status: 'shipped' },
        })
      ).code,
    ).toBe('SCHEMA_VIOLATION');
    // propose_changes surfaces it at dry-run time.
    expect(
      (
        await error('propose_changes', {
          ops: [{ op: 'create', note_id: 'ideas/b', content: 'x', frontmatter: { type: 'idea' } }],
        })
      ).code,
    ).toBe('SCHEMA_VIOLATION');
    // A note that predates the declaration can still be tagged (no NEW
    // problem), but a write that adds a problem is refused.
    await writeFile(join(dir, 'legacy.md'), '---\ntype: idea\n---\nold\n');
    await call('tag', { note_id: 'legacy', tag: 'x' });
    const worse = await error('update_note', {
      note_id: 'legacy',
      frontmatter: { type: 'idea', priority: 'high' },
    });
    expect(worse.reason).toContain('"priority" must be number');
    expect(worse.reason).not.toContain('status');
    // Undeclared types and untyped notes are not validated.
    await call('create_note', { note_id: 'free', content: 'x', frontmatter: { type: 'whatever' } });
  });

  it('capture creates, then appends; tasks are listed; each capture is revertible', async () => {
    const first = await call('capture', { text: 'idea:  link\nthe two notes', date: '2026-11-14' });
    expect(first).toMatchObject({ note_id: 'daily/2026-11-14', created: true });
    const second = await call('capture', {
      text: 'call the bank due:2026-11-20',
      task: true,
      date: '2026-11-14',
    });
    expect(second).toMatchObject({ created: false, line: '- [ ] call the bank due:2026-11-20' });

    const raw = await readFile(join(dir, 'daily', '2026-11-14.md'), 'utf8');
    expect(raw.startsWith('---\ntype: daily\ndate: 2026-11-14\n---\n# 2026-11-14\n\n- ')).toBe(
      true,
    );
    expect(raw).toContain(' idea: link the two notes\n- [ ] call the bank due:2026-11-20\n');

    const tasks = await call('list_tasks', { note_id: 'daily/2026-11-14' });
    expect(tasks.tasks[0].due).toBe('2026-11-20');

    await call('revert', { version_id: second.version_id });
    expect(await readFile(join(dir, 'daily', '2026-11-14.md'), 'utf8')).not.toContain('bank');

    expect((await error('capture', { text: '  ' })).code).toBe('INVALID_INPUT');
    expect((await error('capture', { text: 'x', date: '../../x' })).code).toBe('INVALID_INPUT');
  });

  it('concurrent captures to one day all land (no lost appends)', async () => {
    await Promise.all(
      Array.from({ length: 8 }, (_, i) => call('capture', { text: `n${i}`, date: '2026-11-15' })),
    );
    const raw = await readFile(join(dir, 'daily', '2026-11-15.md'), 'utf8');
    for (let i = 0; i < 8; i += 1) expect(raw).toContain(` n${i}\n`);
  });

  it('capture uses the daily template, filled with the capture date', async () => {
    await mkdir(join(dir, '.tacitus', 'templates'), { recursive: true });
    await writeFile(
      join(dir, '.tacitus', 'templates', 'daily.md'),
      '---\nkind: journal\nday: {{date}}\n---\n# Journal {{date}}\n\n## Log\n',
    );
    await call('capture', { text: 'x', date: '2025-01-02' });
    const raw = await readFile(join(dir, 'daily', '2025-01-02.md'), 'utf8');
    expect(raw).toContain('day: 2025-01-02');
    expect(raw).toContain('# Journal 2025-01-02\n\n## Log\n\n- ');
  });

  it('capture is denied in read-only scope and writes nothing', async () => {
    await build('read-only');
    expect((await error('capture', { text: 'x' })).code).toBe('PERMISSION_DENIED');
    expect(await exists(join(dir, 'daily'))).toBe(false);
  });
});

describe('schema + daily helpers (mirror vault/schema.rs and vault/daily.rs tests)', () => {
  const ARTICLE =
    'description: An article idea\nproperties:\n  status: { type: string, required: true, enum: [idea, draft, published] }\n  priority: { type: number }\n  due: { type: date }\n  topics: { type: list }\n  pinned: { type: boolean }\n';
  const article = parseTypeDef('article', ARTICLE);

  it('conforming notes pass; every violation is listed at once', () => {
    expect(
      violations(article, {
        type: 'article',
        status: 'draft',
        priority: 2,
        due: '2026-11-14',
        topics: ['x'],
        pinned: true,
        extra: 'ok',
      }),
    ).toEqual([]);
    expect(violations(article, { status: 'idea', due: '2026-11-14T10:30:00Z' })).toEqual([]);
    const v = violations(article, {
      priority: 'high',
      due: 'next week',
      topics: 'one',
      pinned: 'yes-ish',
    }).join('; ');
    for (const needle of [
      'missing required property "status"',
      '"priority" must be number, got string "high"',
      '"due" must be date (YYYY-MM-DD)',
      '"topics" must be list',
      '"pinned" must be boolean',
    ]) {
      expect(v).toContain(needle);
    }
    expect(violations(article, { status: 'shipped' })[0]).toContain(
      'must be one of ["idea", "draft", "published"]',
    );
    expect(violations(article, { status: null })[0]).toBe('missing required property "status"');
  });

  it('malformed type files fail loudly; list reports them; .yaml wins over .yml', async () => {
    const dir = await makeVault();
    try {
      const types = join(dir, '.tacitus', 'types');
      await mkdir(types, { recursive: true });
      await writeFile(join(types, 'article.yml'), 'properties: {}\n');
      await writeFile(join(types, 'article.yaml'), ARTICLE);
      await writeFile(join(types, 'bad.yaml'), 'properties:\n  x: { type: colour }\n');
      await writeFile(join(types, 'notes.txt'), 'ignored');
      const store = new TypeStore(dir);
      await expect(store.check('n', { type: 'bad' })).rejects.toMatchObject({
        code: 'INVALID_TYPE_DEF',
      });
      const listed = await store.list();
      expect(listed.map((e) => e.name)).toEqual(['article', 'bad']);
      expect('properties' in listed[0]! && Object.keys(listed[0].properties).length).toBe(5);
      expect('error' in listed[1]!).toBe(true);
      // An unsafe type name is simply "undeclared".
      await expect(store.check('n', { type: '../article' })).resolves.toBeUndefined();
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  it('daily helpers: dates, single-line entries, list-aware appends', () => {
    expect(dailyNoteId('2026-11-14')).toBe('daily/2026-11-14');
    for (const bad of ['2026-13-01', '26-11-14', '../x', '2026-11-14x', '']) {
      expect(() => dailyNoteId(bad)).toThrow(/YYYY-MM-DD/);
    }
    expect(captureLine('  idea\n about  X ', false, '09:05')).toBe('- 09:05 idea about X');
    expect(captureLine('call Ana due:2026-11-20', true, '09:05')).toBe(
      '- [ ] call Ana due:2026-11-20',
    );
    expect(() => captureLine(' \n ', false, '09:05')).toThrow(/empty/);
    expect(appendLine('', '- a')).toBe('- a');
    expect(appendLine('# 2026-11-14\n', '- a')).toBe('# 2026-11-14\n\n- a');
    expect(appendLine('# D\n\n- a\n', '- b')).toBe('# D\n\n- a\n- b');
    expect(appendLine('# D\n\n- [ ] t\n\n', '- b')).toBe('# D\n\n- [ ] t\n- b');
  });
});
