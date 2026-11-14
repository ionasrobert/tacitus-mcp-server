import { TacitusError } from '../lib/errors';

/**
 * Quick capture into a daily note (`daily/YYYY-MM-DD`): pure helpers — the
 * capture tool reads the note, builds the new content here, and commits it
 * through NoteWriter (versioned + audited).
 * Port of crates/tacitus-core/src/vault/daily.rs.
 */

export const DAILY_DIR = 'daily';
export const DAILY_TEMPLATE = 'daily';

const DATE_RE = /^\d{4}-(0[1-9]|1[0-2])-(0[1-9]|[12]\d|3[01])$/;

/** The daily note id for an ISO date, rejecting anything else. */
export function dailyNoteId(date: string): string {
  if (!DATE_RE.test(date)) {
    throw new TacitusError({
      code: 'INVALID_INPUT',
      reason: `date must be YYYY-MM-DD (got ${JSON.stringify(date)}).`,
      suggestion: 'Omit date for today, or pass an ISO date like 2026-11-14.',
    });
  }
  return `${DAILY_DIR}/${date}`;
}

/**
 * The appended line: `- [ ] text` for a task (list_tasks sees it), else
 * `- HH:MM text`. Multi-line text is folded onto one line.
 */
export function captureLine(text: string, task: boolean, hhMm: string): string {
  const folded = text.split(/\s+/).filter(Boolean).join(' ');
  if (folded === '') {
    throw new TacitusError({
      code: 'INVALID_INPUT',
      reason: 'text is empty.',
      suggestion: 'Pass the thought or task to capture in text.',
    });
  }
  return task ? `- [ ] ${folded}` : `- ${hhMm} ${folded}`;
}

/** `content` + `line`: directly under a trailing list, else after a blank line. */
export function appendLine(content: string, line: string): string {
  const body = content.trimEnd();
  if (body === '') return line;
  const last = (body.split('\n').pop() ?? '').trimStart();
  const inList = last.startsWith('- ') || last.startsWith('* ');
  return `${body}${inList ? '\n' : '\n\n'}${line}`;
}
