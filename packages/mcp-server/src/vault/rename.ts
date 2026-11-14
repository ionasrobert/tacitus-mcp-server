import { TacitusError } from '../lib/errors';
import type { VaultIndex } from './index';
import type { Note, WikiLink } from './types';
import type { ChangeOp, Changeset } from './write';

/**
 * Rename with automatic wikilink rewriting — as ONE atomic changeset: create
 * the new note (self-links rewritten), update every note whose links resolve
 * to the old id (alias/heading/block kept, basename and case-insensitive
 * references included), delete the old note. Committed through NoteWriter the
 * whole rename is all-or-nothing, one version, undone by one revert.
 *
 * Pure: only BUILDS the changeset from an index snapshot; the caller commits.
 * Port of crates/tacitus-core/src/vault/rename.rs.
 */

/** Rebuild a wikilink's raw text with a new target, keeping heading/block and alias. */
function rebuildLink(link: WikiLink, newTarget: string): string {
  let s = `[[${newTarget}`;
  if (link.heading !== undefined) s += `#${link.heading}`;
  if (link.block !== undefined) s += `#^${link.block}`;
  if (link.alias !== undefined) s += `|${link.alias}`;
  return `${s}]]`;
}

/** Content with every link resolving to `from` retargeted to `to`; null if unchanged. */
function rewrittenContent(index: VaultIndex, note: Note, from: string, to: string): string | null {
  let content = note.content;
  let changed = false;
  for (const link of note.links) {
    if (index.resolve(link.target)?.id === from) {
      const newRaw = rebuildLink(link, to);
      if (newRaw !== link.raw) {
        content = content.split(link.raw).join(newRaw); // literal replace-all
        changed = true;
      }
    }
  }
  return changed ? content : null;
}

/** Build the atomic changeset that renames `from` to `to` and retargets affected links. */
export function renameNoteOps(index: VaultIndex, from: string, toRaw: string): Changeset {
  const to = toRaw.trim();
  if (to === '' || to === from) {
    throw new TacitusError({
      code: 'INVALID_INPUT',
      reason: `Cannot rename ${JSON.stringify(from)} to ${JSON.stringify(to)}.`,
      suggestion: 'Pass a different, non-empty note id.',
    });
  }
  const note = index.get(from);
  if (!note) {
    throw new TacitusError({
      code: 'NOTE_NOT_FOUND',
      reason: `No note with id ${JSON.stringify(from)}.`,
      suggestion: 'Check the id with list_notes.',
    });
  }
  if (index.get(to)) {
    throw new TacitusError({
      code: 'CONFLICT',
      reason: `Note ${JSON.stringify(to)} already exists.`,
      suggestion: 'Choose a different target id.',
    });
  }

  const ops: ChangeOp[] = [];
  const fm = Object.keys(note.frontmatter).length > 0 ? note.frontmatter : undefined;
  ops.push({
    op: 'create',
    note_id: to,
    content: rewrittenContent(index, note, from, to) ?? note.content,
    frontmatter: fm,
  });
  for (const linking of index.backlinks(from)) {
    const content = rewrittenContent(index, linking, from, to);
    if (content !== null) ops.push({ op: 'update', note_id: linking.id, content });
  }
  ops.push({ op: 'delete', note_id: from });
  return { ops };
}
