import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdtemp, rm, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { NoteWriter } from '../../src/vault/write';
import { parseNote } from '../../src/vault/parse';

describe('NoteWriter convenience + audit (M9)', () => {
  let dir: string;
  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), 'mv-conv-'));
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('createNote writes a note and returns a version_id', async () => {
    const w = new NoteWriter(dir);
    const { version_id } = await w.createNote('notes/a', 'hello', { title: 'A' });
    expect(version_id).toMatch(/^v_/);
    expect(await readFile(join(dir, 'notes', 'a.md'), 'utf8')).toContain('hello');
  });

  it('updateNote replaces content', async () => {
    const w = new NoteWriter(dir);
    await w.createNote('a', 'old');
    await w.updateNote('a', { content: 'new' });
    expect(await readFile(join(dir, 'a.md'), 'utf8')).toContain('new');
  });

  it('link appends a wikilink to the source note', async () => {
    const w = new NoteWriter(dir);
    await w.createNote('a', 'body');
    await w.createNote('b', 'other');
    await w.link('a', 'b');
    const note = parseNote(await readFile(join(dir, 'a.md'), 'utf8'), 'a.md');
    expect(note.links.map((l) => l.target)).toContain('b');
  });

  it('tag adds a frontmatter tag and dedupes', async () => {
    const w = new NoteWriter(dir);
    await w.createNote('a', 'body');
    await w.tag('a', 'urgent');
    await w.tag('a', 'urgent');
    const note = parseNote(await readFile(join(dir, 'a.md'), 'utf8'), 'a.md');
    expect(note.tags).toContain('urgent');
    expect(note.frontmatter.tags).toEqual(['urgent']);
  });

  it('records an audit entry per mutation, most-recent-first', async () => {
    const w = new NoteWriter(dir);
    await w.createNote('a', 'x');
    await w.createNote('b', 'y');
    const audit = await w.readAudit();
    expect(audit.length).toBeGreaterThanOrEqual(2);
    expect(audit[0]?.action).toBe('commit');
    expect(audit[0]?.notes).toContain('b');
  });

  it('read-only scope denies createNote', async () => {
    const w = new NoteWriter(dir, 'read-only');
    await expect(w.createNote('a', 'x')).rejects.toMatchObject({ code: 'PERMISSION_DENIED' });
  });
});
