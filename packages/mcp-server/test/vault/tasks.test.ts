import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { readFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { VaultIndex } from '../../src/vault/index';
import { contentLines, listTasks, toggledContent } from '../../src/vault/tasks';
import { NoteWriter } from '../../src/vault/write';
import { TacitusError } from '../../src/lib/errors';
import { vaultWith } from '../helpers/vault';

function conflictCode(fn: () => unknown): string | undefined {
  try {
    fn();
  } catch (err) {
    return err instanceof TacitusError ? err.code : 'NOT_TACITUS';
  }
  return undefined;
}

describe('tasks (mirrors tasks.rs)', () => {
  let dir: string;
  let index: VaultIndex;
  beforeEach(async () => {
    dir = await vaultWith({
      'today.md':
        '# Today\n\n- [ ] ship the release due:2026-07-25 #launch\n- [x] write tests\n- [ ] water plants\n',
      'plans.md':
        '---\ntitle: Plans\n---\n- [ ] book venue \u{1F4C5} 2026-08-10 #launch\n* [X] draft agenda\nNot a task line.\n',
    });
    index = await VaultIndex.build(dir);
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('parses status, due and tags across notes', () => {
    const all = listTasks(index);
    expect(all).toHaveLength(5);
    const ship = all.find((t) => t.text.includes('ship'))!;
    expect(ship.done).toBe(false);
    expect(ship.due).toBe('2026-07-25');
    expect(ship.tags).toEqual(['launch']);
    expect(ship.line).toBe(2);
    const venue = all.find((t) => t.text.includes('venue'))!;
    expect(venue.due).toBe('2026-08-10');
    expect(all.some((t) => t.text.includes('agenda') && t.done)).toBe(true);
  });

  it('filters and due ordering', () => {
    const open = listTasks(index, { done: false });
    expect(open).toHaveLength(3);
    expect(open[0]!.text).toContain('ship');
    expect(open[1]!.text).toContain('venue');
    expect(open[2]!.due).toBeNull();

    const before = listTasks(index, { due_before: '2026-08-01' });
    expect(before.map((t) => t.text)).toEqual(['ship the release due:2026-07-25 #launch']);
    expect(listTasks(index, { due_after: '2026-08-01' }).map((t) => t.note_id)).toEqual(['plans']);
    expect(listTasks(index, { tag: 'launch' })).toHaveLength(2);
    expect(listTasks(index, { note_id: 'plans' })).toHaveLength(2);
  });

  it('token_budget caps the list', () => {
    const all = listTasks(index);
    const total = all.reduce((n, t) => n + t.token_count, 0);
    const capped = listTasks(index, { token_budget: Math.floor(total / 2) });
    expect(capped.length).toBeGreaterThan(0);
    expect(capped.length).toBeLessThan(all.length);
  });

  it('toggle round-trip is versioned, audited and reflected', async () => {
    const writer = new NoteWriter(dir, 'read-write', index);
    const task = listTasks(index).find((t) => t.text.includes('water'))!;
    const content = toggledContent(
      task.note_id,
      index.get(task.note_id)!.content,
      task.line,
      task.text,
    );
    const { version_id } = await writer.updateNote(task.note_id, { content });
    expect(version_id).toMatch(/^v_/);
    expect(await readFile(join(dir, 'today.md'), 'utf8')).toContain('- [x] water plants');
    expect(listTasks(index).some((t) => t.text.includes('water') && t.done)).toBe(true);
    expect((await writer.readAudit()).length).toBe(1);
  });

  it('a stale, non-task or out-of-range line is a CONFLICT; done flips back', () => {
    const content = '- [ ] real task\nplain line\n';
    expect(conflictCode(() => toggledContent('today', content, 0, 'something else'))).toBe(
      'CONFLICT',
    );
    expect(conflictCode(() => toggledContent('today', content, 1, 'plain line'))).toBe('CONFLICT');
    expect(conflictCode(() => toggledContent('today', content, 99, 'x'))).toBe('CONFLICT');
    expect(toggledContent('t', '- [x] done thing', 0, 'done thing')).toBe('- [ ] done thing');
    expect(toggledContent('t', '- [X] done thing', 0, 'done thing')).toBe('- [ ] done thing');
  });

  it('line indices follow Rust str::lines (CRLF, trailing newline)', () => {
    expect(contentLines('a\r\nb\r\n')).toEqual(['a', 'b']);
    expect(contentLines('a\n\nb')).toEqual(['a', '', 'b']);
    expect(contentLines('')).toEqual([]);
  });
});
