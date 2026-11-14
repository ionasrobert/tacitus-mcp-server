import { TacitusError } from '../lib/errors';
import { estimate } from '../lib/tokens';
import type { VaultIndex } from './index';

/**
 * Tasks as typed entities: checklist lines (`- [ ] text`) anywhere in the
 * vault become queryable entities — status, due date (`due:YYYY-MM-DD` or
 * `📅 YYYY-MM-DD`), inline #tags — instead of agents regexing raw Markdown.
 *
 * Toggling is a PURE function ({@link toggledContent}); the caller commits the
 * result through the transactional NoteWriter (versioned + audited). The
 * expected task text is an optimistic-concurrency guard: a stale caller gets a
 * structured CONFLICT, never a flip of the wrong task.
 * Port of crates/tacitus-core/src/vault/tasks.rs.
 */

export interface Task {
  note_id: string;
  /** 0-based line index within the note's content (frontmatter excluded). */
  line: number;
  /** The text after the checkbox, metadata included. */
  text: string;
  done: boolean;
  /** ISO date from `due:YYYY-MM-DD` or `📅 YYYY-MM-DD`, or null. */
  due: string | null;
  tags: string[];
  token_count: number;
}

export interface TaskFilter {
  /** false = open, true = done, undefined = all. */
  done?: boolean;
  /** Only tasks with a due date strictly before this ISO date. */
  due_before?: string;
  /** Only tasks with a due date on/after this ISO date. */
  due_after?: string;
  tag?: string;
  note_id?: string;
  /** Max tasks (default 100). */
  limit?: number;
  /** Hard token ceiling across returned tasks. */
  token_budget?: number;
}

const TASK_RE = /^\s*[-*] \[([ xX])\] (.*)$/;
const DUE_RE = /(?:due:|\u{1F4C5}\s*)(\d{4}-\d{2}-\d{2})/u;
const TAG_RE = /(?:^|\s)#([A-Za-z0-9_/-]+)/g;
const DEFAULT_LIMIT = 100;

/**
 * Split like Rust's `str::lines`: on `\n`, a trailing `\r` stripped from each
 * line, and no empty final element for a trailing newline — so `line` indices
 * agree with the native engine.
 */
export function contentLines(content: string): string[] {
  if (content === '') return [];
  const lines = content.split('\n');
  if (lines[lines.length - 1] === '') lines.pop();
  return lines.map((l) => (l.endsWith('\r') ? l.slice(0, -1) : l));
}

function parseTaskLine(noteId: string, line: number, raw: string): Task | null {
  const m = TASK_RE.exec(raw);
  if (!m) return null;
  const done = m[1] !== ' ';
  const text = (m[2] ?? '').trimEnd();
  const due = DUE_RE.exec(text)?.[1] ?? null;
  const tags = [...text.matchAll(TAG_RE)].map((t) => t[1] ?? '').filter(Boolean);
  const token_count = estimate(text) + 8; // text + structural overhead
  return { note_id: noteId, line, text, done, due, tags, token_count };
}

/** All tasks in a note's content, in line order. */
export function noteTasks(noteId: string, content: string): Task[] {
  const tasks: Task[] = [];
  contentLines(content).forEach((raw, i) => {
    const task = parseTaskLine(noteId, i, raw);
    if (task) tasks.push(task);
  });
  return tasks;
}

function cmpStr(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

/**
 * Query the vault's tasks. Sorted by due date (missing last), then note_id,
 * then line; capped by `limit` and `token_budget`.
 */
export function listTasks(index: VaultIndex, filter: TaskFilter = {}): Task[] {
  const tasks = index
    .all()
    .filter((n) => filter.note_id === undefined || n.id === filter.note_id)
    .flatMap((n) => noteTasks(n.id, n.content))
    .filter((t) => filter.done === undefined || t.done === filter.done)
    .filter((t) => filter.due_before === undefined || (t.due !== null && t.due < filter.due_before))
    .filter((t) => filter.due_after === undefined || (t.due !== null && t.due >= filter.due_after))
    .filter((t) => filter.tag === undefined || t.tags.includes(filter.tag));

  tasks.sort((a, b) => {
    let c: number;
    if (a.due !== null && b.due !== null) c = cmpStr(a.due, b.due);
    else if (a.due !== null)
      c = -1; // missing due sorts last
    else if (b.due !== null) c = 1;
    else c = 0;
    return c || cmpStr(a.note_id, b.note_id) || a.line - b.line;
  });

  const capped = tasks.slice(0, filter.limit ?? DEFAULT_LIMIT);
  const budget = filter.token_budget ?? Number.POSITIVE_INFINITY;
  let used = 0;
  return capped.filter((t) => {
    if (used + t.token_count > budget) return false;
    used += t.token_count;
    return true;
  });
}

/**
 * Pure toggle: validate the task at `line` in `content` and return the new
 * content with its checkbox flipped. `expectText` is the task text the caller
 * last saw — if the line moved or changed, this is a CONFLICT.
 */
export function toggledContent(
  noteId: string,
  content: string,
  line: number,
  expectText: string,
): string {
  const conflict = (reason: string): TacitusError =>
    new TacitusError({
      code: 'CONFLICT',
      reason,
      suggestion: 'The note changed since you listed tasks; call list_tasks again.',
    });
  const lines = contentLines(content);
  const raw = lines[line];
  if (raw === undefined) {
    throw conflict(`Line ${line} no longer exists in ${JSON.stringify(noteId)}.`);
  }
  const task = parseTaskLine(noteId, line, raw);
  if (!task) throw conflict(`Line ${line} in ${JSON.stringify(noteId)} is not a task.`);
  if (task.text !== expectText) {
    throw conflict(
      `Task at line ${line} is now ${JSON.stringify(task.text)}, expected ${JSON.stringify(expectText)}.`,
    );
  }
  lines[line] = task.done
    ? raw.replace('[x]', '[ ]').replace('[X]', '[ ]')
    : raw.replace('[ ]', '[x]');
  return lines.join('\n');
}
