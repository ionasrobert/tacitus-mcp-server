import { appendFile, mkdir, readFile, rename, rm, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { stringify as stringifyYaml } from 'yaml';
import { TacitusError } from '../lib/errors';
import { stableId } from '../lib/ids';
import type { PermissionScope } from '../tools/types';
import type { VaultIndex } from './index';
import { parseNote } from './parse';
import type { Note } from './types';

export interface CreateOp {
  op: 'create';
  note_id: string;
  content: string;
  frontmatter?: Record<string, unknown>;
}
export interface UpdateOp {
  op: 'update';
  note_id: string;
  content?: string;
  frontmatter?: Record<string, unknown>;
}
export interface DeleteOp {
  op: 'delete';
  note_id: string;
}
export type ChangeOp = CreateOp | UpdateOp | DeleteOp;
export interface Changeset {
  ops: ChangeOp[];
}

export interface DiffEntry {
  note_id: string;
  op: ChangeOp['op'];
  before: string | null;
  after: string | null;
}
export interface Proposal {
  change_id: string;
  diff: DiffEntry[];
}
export interface CommitResult {
  version_id: string;
}

interface Snapshot {
  version_id: string;
  change_id: string;
  /** note_id → prior raw file contents, or null if the note did not exist. */
  before: Record<string, string | null>;
  /** note_id → new raw file contents, or null if the note was deleted. */
  after: Record<string, string | null>;
}

export interface AuditEntry {
  ts: string;
  action: 'commit' | 'revert';
  version_id: string;
  change_id?: string;
  notes: string[];
  scope: PermissionScope;
}

function serialize(frontmatter: Record<string, unknown> | undefined, content: string): string {
  if (frontmatter && Object.keys(frontmatter).length > 0) {
    return `---\n${stringifyYaml(frontmatter).trimEnd()}\n---\n${content}\n`;
  }
  return `${content}\n`;
}

async function readRawOrNull(path: string): Promise<string | null> {
  try {
    return await readFile(path, 'utf8');
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === 'ENOENT') return null;
    throw err;
  }
}

/**
 * Transactional write-back for vault notes (Part I.4): a changeset is validated
 * and previewed with propose() (no disk mutation), applied atomically with
 * commit(), and undoable with revert(). Read-only scope forbids mutation.
 */
export class NoteWriter {
  private readonly tacitusDir: string;
  private readonly historyDir: string;
  private readonly auditPath: string;
  private readonly pending = new Map<string, Changeset>();

  constructor(
    private readonly vaultDir: string,
    private readonly scope: PermissionScope = 'read-write',
    private readonly index?: VaultIndex,
  ) {
    this.tacitusDir = join(vaultDir, '.tacitus');
    this.historyDir = join(this.tacitusDir, 'history');
    this.auditPath = join(this.tacitusDir, 'audit.log');
  }

  private notePath(noteId: string): string {
    return join(this.vaultDir, `${noteId}.md`);
  }

  // ---- Convenience helpers (auto-commit; still transactional + audited) ----

  async createNote(
    noteId: string,
    content: string,
    frontmatter?: Record<string, unknown>,
  ): Promise<CommitResult> {
    return this.apply({ ops: [{ op: 'create', note_id: noteId, content, frontmatter }] });
  }

  async updateNote(
    noteId: string,
    patch: { content?: string; frontmatter?: Record<string, unknown> },
  ): Promise<CommitResult> {
    return this.apply({
      ops: [{ op: 'update', note_id: noteId, content: patch.content, frontmatter: patch.frontmatter }],
    });
  }

  /** Append a `[[to]]` wikilink to the source note (idempotent). */
  async link(from: string, to: string): Promise<CommitResult> {
    const note = await this.currentNote(from);
    if (!note) throw this.notFound(from);
    const marker = `[[${to}]]`;
    const content = note.content.includes(marker)
      ? note.content
      : `${note.content.trimEnd()}\n\n${marker}\n`;
    return this.apply({ ops: [{ op: 'update', note_id: from, content }] });
  }

  /** Add a tag to the note's frontmatter (deduplicated). */
  async tag(noteId: string, tag: string): Promise<CommitResult> {
    const note = await this.currentNote(noteId);
    if (!note) throw this.notFound(noteId);
    const existing = Array.isArray(note.frontmatter.tags)
      ? note.frontmatter.tags.map(String)
      : [];
    const tags = existing.includes(tag) ? existing : [...existing, tag];
    return this.apply({
      ops: [{ op: 'update', note_id: noteId, frontmatter: { ...note.frontmatter, tags } }],
    });
  }

  private async apply(changeset: Changeset): Promise<CommitResult> {
    const { change_id } = await this.propose(changeset);
    return this.commit(change_id);
  }

  private async currentNote(noteId: string): Promise<Note | null> {
    const raw = await readRawOrNull(this.notePath(noteId));
    return raw === null ? null : parseNote(raw, `${noteId}.md`);
  }

  /** Dry-run: validate the changeset and return a before/after diff. No writes. */
  async propose(changeset: Changeset): Promise<Proposal> {
    const diff = await this.buildDiff(changeset);
    const change_id = stableId(JSON.stringify(changeset), 'chg');
    this.pending.set(change_id, changeset);
    return { change_id, diff };
  }

  async commit(change_id: string): Promise<CommitResult> {
    this.assertWritable();
    const changeset = this.pending.get(change_id);
    if (!changeset) {
      throw new TacitusError({
        code: 'UNKNOWN_CHANGE',
        reason: `No pending change with id "${change_id}".`,
        suggestion: 'Call propose_changes first and use the returned change_id.',
      });
    }

    // Re-validate against current disk state (may have changed since propose).
    const diff = await this.buildDiff(changeset);

    const before: Record<string, string | null> = {};
    const after: Record<string, string | null> = {};
    for (const entry of diff) {
      before[entry.note_id] = entry.before;
      after[entry.note_id] = entry.after;
    }

    // Apply atomically: on any failure, roll back everything from `before`.
    const done: string[] = [];
    try {
      for (const entry of diff) {
        await this.applyState(entry.note_id, entry.after);
        done.push(entry.note_id);
      }
    } catch (err) {
      for (const noteId of done) await this.applyState(noteId, before[noteId] ?? null);
      throw err;
    }

    const version_id = stableId(`${change_id}:${Date.now()}`, 'v');
    await this.writeSnapshot({ version_id, change_id, before, after });
    this.reflect(after);
    await this.appendAudit({
      ts: new Date().toISOString(),
      action: 'commit',
      version_id,
      change_id,
      notes: Object.keys(after),
      scope: this.scope,
    });
    this.pending.delete(change_id);
    return { version_id };
  }

  async revert(version_id: string): Promise<{ reverted: true; version_id: string }> {
    this.assertWritable();
    const raw = await readRawOrNull(join(this.historyDir, `${version_id}.json`));
    if (raw === null) {
      throw new TacitusError({
        code: 'UNKNOWN_VERSION',
        reason: `No version with id "${version_id}".`,
        suggestion: 'Use a version_id returned by commit_changes.',
      });
    }
    const snapshot = JSON.parse(raw) as Snapshot;
    for (const [noteId, priorRaw] of Object.entries(snapshot.before)) {
      await this.applyState(noteId, priorRaw);
    }
    this.reflect(snapshot.before);
    await this.appendAudit({
      ts: new Date().toISOString(),
      action: 'revert',
      version_id,
      notes: Object.keys(snapshot.before),
      scope: this.scope,
    });
    return { reverted: true, version_id };
  }

  /** Read the agent-action audit log, most recent first. */
  async readAudit(limit = 50): Promise<AuditEntry[]> {
    const raw = await readRawOrNull(this.auditPath);
    if (!raw) return [];
    const entries = raw
      .split('\n')
      .filter(Boolean)
      .map((line) => JSON.parse(line) as AuditEntry);
    return entries.reverse().slice(0, limit);
  }

  private async appendAudit(entry: AuditEntry): Promise<void> {
    await mkdir(this.tacitusDir, { recursive: true });
    await appendFile(this.auditPath, `${JSON.stringify(entry)}\n`, 'utf8');
  }

  private assertWritable(): void {
    if (this.scope === 'read-only') {
      throw new TacitusError({
        code: 'PERMISSION_DENIED',
        reason: 'This session is read-only; writes are not permitted.',
        suggestion: 'Re-open the vault with a read-write scope to apply changes.',
      });
    }
  }

  private async buildDiff(changeset: Changeset): Promise<DiffEntry[]> {
    const diff: DiffEntry[] = [];
    for (const op of changeset.ops) {
      const path = this.notePath(op.note_id);
      const before = await readRawOrNull(path);

      if (op.op === 'create') {
        if (before !== null) {
          throw new TacitusError({
            code: 'CONFLICT',
            reason: `Note "${op.note_id}" already exists.`,
            suggestion: 'Use an update op, or choose a different note_id.',
          });
        }
        diff.push({
          note_id: op.note_id,
          op: 'create',
          before: null,
          after: serialize(op.frontmatter, op.content),
        });
      } else if (op.op === 'update') {
        if (before === null) throw this.notFound(op.note_id);
        const existing = parseNote(before, `${op.note_id}.md`);
        const after = serialize(
          op.frontmatter ?? existing.frontmatter,
          op.content ?? existing.content,
        );
        diff.push({ note_id: op.note_id, op: 'update', before, after });
      } else {
        if (before === null) throw this.notFound(op.note_id);
        diff.push({ note_id: op.note_id, op: 'delete', before, after: null });
      }
    }
    return diff;
  }

  private notFound(noteId: string): TacitusError {
    return new TacitusError({
      code: 'NOTE_NOT_FOUND',
      reason: `No note with id "${noteId}".`,
      suggestion: 'Create it first, or check the id with list_notes.',
    });
  }

  /** Bring a note file to the given state (null = deleted) with an atomic write. */
  private async applyState(noteId: string, raw: string | null): Promise<void> {
    const path = this.notePath(noteId);
    if (raw === null) {
      await rm(path, { force: true });
      return;
    }
    await mkdir(dirname(path), { recursive: true });
    const tmp = `${path}.tmp`;
    await writeFile(tmp, raw, 'utf8');
    await rename(tmp, path);
  }

  private reflect(state: Record<string, string | null>): void {
    if (!this.index) return;
    for (const [noteId, raw] of Object.entries(state)) {
      if (raw === null) this.index.removeNote(noteId);
      else this.index.upsertRaw(`${noteId}.md`, raw);
    }
  }

  private async writeSnapshot(snapshot: Snapshot): Promise<void> {
    await mkdir(this.historyDir, { recursive: true });
    const path = join(this.historyDir, `${snapshot.version_id}.json`);
    const tmp = `${path}.tmp`;
    await writeFile(tmp, JSON.stringify(snapshot, null, 2), 'utf8');
    await rename(tmp, path);
  }
}
