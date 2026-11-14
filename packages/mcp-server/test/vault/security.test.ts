import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdtemp, mkdir, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { assertNoteId, noteIdProblem } from '../../src/lib/ids';
import { MemoryStore } from '../../src/memory/store';
import { NoteWriter } from '../../src/vault/write';

const BAD_IDS = [
  '../x',
  '../../outside/x',
  '/etc/x',
  '.tacitus/audit',
  '.tacitus/history/v_1',
  'a//b',
  'a/./b',
  'a/..',
  'C:/x',
  'a\\..\\x',
  '',
  'x\0y',
];

/** Every file under `dir`, relative, recursively. */
async function tree(dir: string): Promise<string[]> {
  return ((await readdir(dir, { recursive: true })) as string[]).sort();
}

describe('note id validation (no writes outside the vault)', () => {
  let root: string; // parent dir: anything escaping the vault lands here
  let vault: string;
  let writer: NoteWriter;
  beforeEach(async () => {
    root = await mkdtemp(join(tmpdir(), 'mv-sec-'));
    vault = join(root, 'vault');
    await mkdir(join(vault, '.tacitus'), { recursive: true });
    await writeFile(join(vault, 'ok.md'), 'fine\n');
    await writeFile(join(root, 'x.md'), 'outside, must survive\n');
    writer = new NoteWriter(vault);
  });
  afterEach(async () => {
    await rm(root, { recursive: true, force: true });
  });

  it.each(BAD_IDS)('rejects %j for every write path, touching nothing', async (id) => {
    const before = await tree(root);
    const attempts = [
      writer.createNote(id, 'pwned'),
      writer.updateNote(id, { content: 'pwned' }),
      writer.deleteNote(id),
      writer.tag(id, 't'),
      writer.link(id, 'ok'),
      writer.propose({ ops: [{ op: 'create', note_id: id, content: 'pwned' }] }),
    ];
    for (const attempt of attempts) {
      await expect(attempt).rejects.toMatchObject({ code: 'INVALID_NOTE_ID' });
    }
    expect(await tree(root)).toEqual(before);
    expect(await readFile(join(root, 'x.md'), 'utf8')).toBe('outside, must survive\n');
  });

  it('one bad op makes the whole changeset fail before any write', async () => {
    await expect(
      writer.propose({
        ops: [
          { op: 'create', note_id: 'fresh', content: 'a' },
          { op: 'delete', note_id: '../x' },
        ],
      }),
    ).rejects.toMatchObject({ code: 'INVALID_NOTE_ID' });
    expect(await tree(vault)).not.toContain('fresh.md');
  });

  it('accepts ordinary nested, dotted and hidden-folder ids', () => {
    for (const id of [
      'a',
      'projects/launch',
      'notes/v1.2 plan',
      '.obsidian-like/x',
      'Meeting: notes',
      'a/b/c',
    ]) {
      expect(noteIdProblem(id)).toBeNull();
      expect(() => assertNoteId(id)).not.toThrow();
    }
  });

  it('the error is structured and actionable', () => {
    try {
      assertNoteId('../x');
      expect.unreachable();
    } catch (err) {
      expect(err).toMatchObject({
        code: 'INVALID_NOTE_ID',
        reason: expect.stringContaining('"../x"'),
        suggestion: expect.stringContaining('vault-relative'),
      });
    }
  });

  it('version ids are plain tokens: traversal reads as UNKNOWN_VERSION', async () => {
    await writeFile(
      join(vault, 'evil.json'),
      JSON.stringify({ version_id: 'x', change_id: 'y', before: { ok: null }, after: {} }),
    );
    for (const v of ['../evil', '../../vault/evil', 'v_1/../../evil']) {
      await expect(writer.revert(v)).rejects.toMatchObject({ code: 'UNKNOWN_VERSION' });
      await expect(writer.getVersion(v)).rejects.toMatchObject({ code: 'UNKNOWN_VERSION' });
    }
    expect(await readFile(join(vault, 'ok.md'), 'utf8')).toBe('fine\n'); // never "reverted"
  });

  it('memory ids are plain tokens: forget cannot delete outside the store', async () => {
    const store = new MemoryStore(vault);
    for (const id of ['../../ok', '../../../x', 'a/b']) {
      await expect(store.remove(id)).rejects.toMatchObject({ code: 'INVALID_INPUT' });
    }
    expect(await readFile(join(vault, 'ok.md'), 'utf8')).toBe('fine\n');
    expect(await readFile(join(root, 'x.md'), 'utf8')).toBe('outside, must survive\n');
    expect(await store.remove('mem_0123456789abcdef')).toBe(false); // valid shape, absent
  });
});
