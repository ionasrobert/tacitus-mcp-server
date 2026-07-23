import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdtemp, rm, writeFile, readFile, access } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { NoteWriter } from '../../src/vault/write';
import { VaultIndex } from '../../src/vault/index';
import { TacitusError } from '../../src/lib/errors';

async function exists(p: string): Promise<boolean> {
  try {
    await access(p);
    return true;
  } catch {
    return false;
  }
}

describe('NoteWriter (M7: transactional write-back)', () => {
  let dir: string;
  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), 'mv-write-'));
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('propose is a dry-run: returns a diff, touches no files', async () => {
    const writer = new NoteWriter(dir);
    const { change_id, diff } = await writer.propose({
      ops: [{ op: 'create', note_id: 'notes/new', content: 'hello' }],
    });
    expect(change_id).toMatch(/^chg_/);
    expect(diff).toHaveLength(1);
    expect(diff[0]).toMatchObject({ note_id: 'notes/new', op: 'create', before: null });
    expect(diff[0]?.after).toContain('hello');
    expect(await exists(join(dir, 'notes', 'new.md'))).toBe(false);
  });

  it('commit applies the changeset to disk', async () => {
    const writer = new NoteWriter(dir);
    const { change_id } = await writer.propose({
      ops: [{ op: 'create', note_id: 'a', content: 'body', frontmatter: { title: 'A' } }],
    });
    const { version_id } = await writer.commit(change_id);
    expect(version_id).toMatch(/^v_/);
    const raw = await readFile(join(dir, 'a.md'), 'utf8');
    expect(raw).toContain('title: A');
    expect(raw).toContain('body');
  });

  it('rejects creating over an existing note (CONFLICT) at propose', async () => {
    await writeFile(join(dir, 'a.md'), 'existing', 'utf8');
    const writer = new NoteWriter(dir);
    await expect(
      writer.propose({ ops: [{ op: 'create', note_id: 'a', content: 'x' }] }),
    ).rejects.toMatchObject({ code: 'CONFLICT' });
  });

  it('rejects a missing-note update and stays all-or-nothing', async () => {
    const writer = new NoteWriter(dir);
    await expect(
      writer.propose({
        ops: [
          { op: 'create', note_id: 'ok', content: 'x' },
          { op: 'update', note_id: 'missing', content: 'y' },
        ],
      }),
    ).rejects.toMatchObject({ code: 'NOTE_NOT_FOUND' });
    expect(await exists(join(dir, 'ok.md'))).toBe(false);
  });

  it('revert removes a note that a create introduced', async () => {
    const writer = new NoteWriter(dir);
    const { change_id } = await writer.propose({
      ops: [{ op: 'create', note_id: 'temp', content: 'x' }],
    });
    const { version_id } = await writer.commit(change_id);
    expect(await exists(join(dir, 'temp.md'))).toBe(true);
    await writer.revert(version_id);
    expect(await exists(join(dir, 'temp.md'))).toBe(false);
  });

  it('revert restores prior content after an update', async () => {
    await writeFile(join(dir, 'doc.md'), 'OLD', 'utf8');
    const writer = new NoteWriter(dir);
    const { change_id } = await writer.propose({
      ops: [{ op: 'update', note_id: 'doc', content: 'NEW' }],
    });
    const { version_id } = await writer.commit(change_id);
    expect(await readFile(join(dir, 'doc.md'), 'utf8')).toContain('NEW');
    await writer.revert(version_id);
    expect(await readFile(join(dir, 'doc.md'), 'utf8')).toBe('OLD');
  });

  it('propose is idempotent: same changeset ⇒ same change_id', async () => {
    const writer = new NoteWriter(dir);
    const cs = { ops: [{ op: 'create' as const, note_id: 'z', content: 'zz' }] };
    expect((await writer.propose(cs)).change_id).toBe((await writer.propose(cs)).change_id);
  });

  it('read-only scope denies commit but allows propose (dry-run)', async () => {
    const writer = new NoteWriter(dir, 'read-only');
    const { change_id } = await writer.propose({
      ops: [{ op: 'create', note_id: 'x', content: 'x' }],
    });
    await expect(writer.commit(change_id)).rejects.toBeInstanceOf(TacitusError);
    await expect(writer.commit(change_id)).rejects.toMatchObject({ code: 'PERMISSION_DENIED' });
  });

  it('reflects committed changes into a provided live index', async () => {
    const index = await VaultIndex.build(dir);
    const writer = new NoteWriter(dir, 'read-write', index);
    const { change_id } = await writer.propose({
      ops: [{ op: 'create', note_id: 'live', content: 'live note' }],
    });
    await writer.commit(change_id);
    expect(index.get('live')?.title).toBe('live');
  });
});
