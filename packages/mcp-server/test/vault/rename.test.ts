import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { access, readFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { VaultIndex } from '../../src/vault/index';
import { renameNoteOps } from '../../src/vault/rename';
import { NoteWriter } from '../../src/vault/write';
import { TacitusError } from '../../src/lib/errors';
import { vaultWith } from '../helpers/vault';

async function exists(p: string): Promise<boolean> {
  try {
    await access(p);
    return true;
  } catch {
    return false;
  }
}

function codeOf(fn: () => unknown): string | undefined {
  try {
    fn();
  } catch (err) {
    return err instanceof TacitusError ? err.code : 'NOT_TACITUS';
  }
  return undefined;
}

describe('rename_note (mirrors rename.rs)', () => {
  let dir: string;
  let index: VaultIndex;
  let writer: NoteWriter;
  beforeEach(async () => {
    dir = await vaultWith({
      'projects/launch.md':
        '---\ntitle: Launch\npriority: 3\n---\nSee [[projects/launch]] self-link and [[ideas]].\n',
      'a.md': 'Full id [[projects/launch]] and alias [[projects/launch|the launch]].\n',
      'b.md': 'Basename case-insensitive [[Launch#Timeline]] link.\n',
      'ideas.md': '# Ideas\nUnrelated [[a]].\n',
    });
    index = await VaultIndex.build(dir);
    writer = new NoteWriter(dir, 'read-write', index);
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('moves the note and rewrites every link shape in one changeset', async () => {
    const changeset = renameNoteOps(index, 'projects/launch', 'projects/rocket');
    expect(changeset.ops.map((o) => o.op)).toEqual(['create', 'update', 'update', 'delete']);
    await writer.apply(changeset);

    expect(await exists(join(dir, 'projects/launch.md'))).toBe(false);
    const moved = await readFile(join(dir, 'projects/rocket.md'), 'utf8');
    expect(moved).toContain('priority: 3');
    expect(moved).toContain('[[projects/rocket]] self-link');
    const a = await readFile(join(dir, 'a.md'), 'utf8');
    expect(a).toContain('[[projects/rocket]]');
    expect(a).toContain('[[projects/rocket|the launch]]');
    expect(await readFile(join(dir, 'b.md'), 'utf8')).toContain('[[projects/rocket#Timeline]]');

    expect(index.get('projects/launch')).toBeUndefined();
    const backlinks = index.backlinks('projects/rocket').map((n) => n.id);
    expect(backlinks).toEqual(expect.arrayContaining(['a', 'b']));
  });

  it('one revert undoes the entire rename', async () => {
    const { version_id } = await writer.apply(
      renameNoteOps(index, 'projects/launch', 'projects/rocket'),
    );
    await writer.revert(version_id);
    expect(await exists(join(dir, 'projects/launch.md'))).toBe(true);
    expect(await exists(join(dir, 'projects/rocket.md'))).toBe(false);
    expect(await readFile(join(dir, 'a.md'), 'utf8')).toContain('[[projects/launch|the launch]]');
    expect(await readFile(join(dir, 'b.md'), 'utf8')).toContain('[[Launch#Timeline]]');
    expect(index.get('projects/launch')).toBeDefined();
  });

  it('guards: conflict, missing, no-op', () => {
    expect(codeOf(() => renameNoteOps(index, 'projects/launch', 'ideas'))).toBe('CONFLICT');
    expect(codeOf(() => renameNoteOps(index, 'nope', 'x'))).toBe('NOTE_NOT_FOUND');
    expect(codeOf(() => renameNoteOps(index, 'ideas', 'ideas'))).toBe('INVALID_INPUT');
    expect(codeOf(() => renameNoteOps(index, 'ideas', '   '))).toBe('INVALID_INPUT');
  });

  it('a rename target outside the vault is rejected before anything is written', async () => {
    const changeset = renameNoteOps(index, 'ideas', '../escaped');
    await expect(writer.apply(changeset)).rejects.toMatchObject({ code: 'INVALID_NOTE_ID' });
    expect(await exists(join(dir, 'ideas.md'))).toBe(true);
    expect(await exists(join(dir, '..', 'escaped.md'))).toBe(false);
  });
});
