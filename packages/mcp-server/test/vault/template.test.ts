import { describe, it, expect, afterEach } from 'vitest';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { parseNote } from '../../src/vault/parse';
import { builtinValue, renderTemplate, TemplateStore } from '../../src/vault/template';
import { TacitusError } from '../../src/lib/errors';

async function codeOf(p: Promise<unknown> | (() => unknown)): Promise<string | undefined> {
  try {
    if (typeof p === 'function') p();
    else await p;
  } catch (err) {
    return err instanceof TacitusError ? err.code : 'NOT_TACITUS';
  }
  return undefined;
}

describe('templates (mirrors template.rs)', () => {
  let dir: string | undefined;
  afterEach(async () => {
    if (dir) await rm(dir, { recursive: true, force: true });
    dir = undefined;
  });

  it('renders vars and auto-fills the builtin date', () => {
    const out = renderTemplate('# {{title}}\n\nCreated: {{date}}\n', { title: 'Standup' });
    expect(out).toContain('# Standup');
    expect(out).toMatch(/Created: \d{4}-\d{2}-\d{2}\n/);
  });

  it('builtins use local time in chrono formats', () => {
    const now = new Date(2026, 10, 4, 9, 5, 7, 42);
    expect(builtinValue('date', now)).toBe('2026-11-04');
    expect(builtinValue('time', now)).toBe('09:05');
    expect(builtinValue('datetime', now)).toMatch(/^2026-11-04T09:05:07\.042[+-]\d{2}:\d{2}$/);
  });

  it('missing vars is a structured error naming them (never builtins)', async () => {
    let err: TacitusError | undefined;
    try {
      renderTemplate('{{title}} by {{author}} on {{date}}', { title: 'x' });
    } catch (e) {
      err = e as TacitusError;
    }
    expect(err?.code).toBe('MISSING_VARS');
    expect(err?.reason).toContain('author');
    expect(err?.reason).not.toContain('date');
  });

  it('substitution before parsing preserves YAML types', () => {
    const raw = '---\ntitle: "{{title}}"\npriority: {{p}}\ndone: false\n---\nBody {{title}}.\n';
    const note = parseNote(renderTemplate(raw, { title: 'Plan', p: '3' }), 'x.md');
    expect(note.frontmatter.priority).toBe(3);
    expect(note.frontmatter.done).toBe(false);
    expect(note.title).toBe('Plan');
    expect(note.content).toContain('Body Plan.');
  });

  it('a supplied var wins over a builtin; $-patterns are literal', () => {
    expect(renderTemplate('{{date}} {{x}}', { date: 'D', x: '$& $1' })).toBe('D $& $1');
  });

  it('store lists templates with inferred vars and rejects escaping names', async () => {
    dir = await mkdtemp(join(tmpdir(), 'mv-tpl-'));
    const tplDir = join(dir, '.tacitus', 'templates');
    await mkdir(tplDir, { recursive: true });
    await writeFile(
      join(tplDir, 'meeting.md'),
      '---\ntags: [meeting]\n---\n# {{title}}\n{{date}} with {{attendees}} re {{title}}\n',
    );
    await writeFile(join(tplDir, 'blank.md'), 'Nothing dynamic.\n');
    await writeFile(join(tplDir, 'notes.txt'), '{{ignored}}');
    const store = new TemplateStore(dir);
    expect(await store.list()).toEqual([
      { name: 'blank', vars: [] },
      { name: 'meeting', vars: ['title', 'attendees'] },
    ]);
    expect(await codeOf(store.loadRaw('nope'))).toBe('TEMPLATE_NOT_FOUND');
    expect(await codeOf(store.loadRaw('../../etc/passwd'))).toBe('TEMPLATE_NOT_FOUND');
  });

  it('a vault without templates lists none', async () => {
    dir = await mkdtemp(join(tmpdir(), 'mv-tpl-'));
    expect(await new TemplateStore(dir).list()).toEqual([]);
  });
});
