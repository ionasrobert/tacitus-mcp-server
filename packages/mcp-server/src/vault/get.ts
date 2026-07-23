import { stringify as stringifyYaml } from 'yaml';
import { TacitusError } from '../lib/errors';
import { estimate } from '../lib/tokens';
import type { VaultIndex } from './index';

export type NoteFormat = 'outline' | 'frontmatter_only' | 'full';

export interface GetNoteArgs {
  format?: NoteFormat;
  max_tokens?: number;
}

export interface GetNoteResult {
  note_id: string;
  title: string;
  format: NoteFormat;
  content: string;
  token_count: number;
  truncated: boolean;
}

/** Truncate to roughly max_tokens (≈ 4 chars/token) on a word boundary. */
function truncate(text: string, maxTokens: number): { text: string; truncated: boolean } {
  const maxChars = maxTokens * 4;
  if (text.length <= maxChars) return { text, truncated: false };
  const cut = text.slice(0, maxChars);
  const lastSpace = cut.lastIndexOf(' ');
  return { text: (lastSpace > 0 ? cut.slice(0, lastSpace) : cut).trimEnd(), truncated: true };
}

/**
 * Progressive disclosure (Part I.1): outline (headings) → frontmatter → full.
 * `full` respects max_tokens so a single note can't blow the context window.
 */
export function getNote(index: VaultIndex, id: string, args: GetNoteArgs = {}): GetNoteResult {
  const note = index.get(id);
  if (!note) {
    throw new TacitusError({
      code: 'NOTE_NOT_FOUND',
      reason: `No note with id "${id}".`,
      suggestion: 'Use search or list_notes to discover valid note ids.',
    });
  }

  const format = args.format ?? 'outline';
  let content: string;
  if (format === 'outline') {
    content = note.headings.map((h) => `${'  '.repeat(h.level - 1)}- ${h.text}`).join('\n');
  } else if (format === 'frontmatter_only') {
    content = stringifyYaml(note.frontmatter).trimEnd();
  } else {
    content = note.content;
  }

  let truncated = false;
  if (args.max_tokens !== undefined) {
    const result = truncate(content, args.max_tokens);
    content = result.text;
    truncated = result.truncated;
  }

  return {
    note_id: note.id,
    title: note.title,
    format,
    content,
    token_count: estimate(content),
    truncated,
  };
}
