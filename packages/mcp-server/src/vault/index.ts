import { readdir, readFile } from 'node:fs/promises';
import { join, sep } from 'node:path';
import { parseNote } from './parse';
import type { Note } from './types';

/**
 * An in-memory snapshot of a vault's notes plus its wikilink graph.
 * Built once at startup; file-watching / incremental updates come later.
 */
export class VaultIndex {
  private constructor(private readonly notes: Map<string, Note>) {}

  static async build(vaultDir: string): Promise<VaultIndex> {
    let entries: string[] = [];
    try {
      entries = (await readdir(vaultDir, { recursive: true })) as string[];
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err;
    }

    const notes = new Map<string, Note>();
    for (const entry of entries) {
      const relPath = entry.split(sep).join('/');
      if (!relPath.endsWith('.md')) continue;
      if (relPath.startsWith('.tacitus/')) continue;
      const raw = await readFile(join(vaultDir, entry), 'utf8');
      const note = parseNote(raw, relPath);
      notes.set(note.id, note);
    }
    return new VaultIndex(notes);
  }

  all(): Note[] {
    return [...this.notes.values()];
  }

  get(id: string): Note | undefined {
    return this.notes.get(id);
  }

  /** Reflect a written note into the live index (used after a commit/revert). */
  upsertRaw(relPath: string, raw: string): void {
    const note = parseNote(raw, relPath);
    this.notes.set(note.id, note);
  }

  removeNote(id: string): void {
    this.notes.delete(id);
  }

  /** Resolve a wikilink target to a note: exact id first, then basename. */
  resolve(target: string): Note | undefined {
    if (this.notes.has(target)) return this.notes.get(target);
    const wanted = target.toLowerCase();
    for (const note of this.notes.values()) {
      if ((note.id.split('/').pop() ?? note.id).toLowerCase() === wanted) return note;
    }
    return undefined;
  }

  /** Notes that the given note links to (resolved, de-duplicated). */
  outgoing(id: string): Note[] {
    const note = this.notes.get(id);
    if (!note) return [];
    const seen = new Set<string>();
    const out: Note[] = [];
    for (const link of note.links) {
      const target = this.resolve(link.target);
      if (target && target.id !== id && !seen.has(target.id)) {
        seen.add(target.id);
        out.push(target);
      }
    }
    return out;
  }

  /** Notes that link to the given note. */
  backlinks(id: string): Note[] {
    if (!this.notes.has(id)) return [];
    const out: Note[] = [];
    for (const note of this.notes.values()) {
      if (note.id === id) continue;
      if (note.links.some((l) => this.resolve(l.target)?.id === id)) out.push(note);
    }
    return out;
  }
}
