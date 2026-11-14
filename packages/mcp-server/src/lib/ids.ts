import { createHash } from 'node:crypto';
import { TacitusError } from './errors';

/**
 * Deterministic, stable id from a seed string.
 *
 * Stable ids are the backbone of idempotency (Part I.4): re-remembering the
 * same fact from the same source yields the same id, so no duplicates.
 */
export function stableId(seed: string, prefix = 'mem'): string {
  const digest = createHash('sha256').update(seed).digest('hex').slice(0, 16);
  return `${prefix}_${digest}`;
}

const NOTE_ID_SUGGESTION =
  'Use a vault-relative id like "projects/launch" (no leading /, no .. segments, not inside .tacitus).';

/** Why a note id is unsafe, or null when it is a valid vault-relative id. */
export function noteIdProblem(noteId: string): string | null {
  if (noteId.length === 0) return 'is empty';
  if (noteId.startsWith('/')) return 'is absolute';
  if (noteId.includes('\\')) return 'contains a backslash';
  if (noteId.includes('\0')) return 'contains a NUL byte';
  if (/^[A-Za-z]:/.test(noteId)) return 'starts with a drive letter';
  const segments = noteId.split('/');
  if (segments.some((s) => s === '' || s === '.' || s === '..')) {
    return 'has an empty, "." or ".." path segment';
  }
  if (segments[0] === '.tacitus') return 'points inside .tacitus';
  return null;
}

/**
 * Reject note ids that would resolve outside the vault (or into Tacitus'
 * internal `.tacitus/` state). Every filesystem access by note id goes
 * through this — mirrored by `validate_note_id` in the Rust engine.
 */
export function assertNoteId(noteId: string): void {
  const problem = noteIdProblem(noteId);
  if (problem !== null) {
    throw new TacitusError({
      code: 'INVALID_NOTE_ID',
      reason: `Note id ${JSON.stringify(noteId)} ${problem}.`,
      suggestion: NOTE_ID_SUGGESTION,
    });
  }
}

/** Generated ids (memory_id, version_id, change_id) are `prefix_hex` — plain tokens. */
export function isPlainId(id: string): boolean {
  return /^[A-Za-z0-9_-]+$/.test(id);
}
