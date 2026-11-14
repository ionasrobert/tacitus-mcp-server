import { z } from 'zod';
import { appendLine, captureLine, DAILY_TEMPLATE, dailyNoteId } from '../vault/daily';
import { parseNote } from '../vault/parse';
import { TypeStore, typeToJson } from '../vault/schema';
import { builtinValue, renderTemplate, TemplateStore } from '../vault/template';
import type { NoteWriter } from '../vault/write';
import type { Tool } from './types';

/**
 * Declared types and quick capture. Descriptions are byte-identical to
 * crates/tacitus-core/src/tools/mod.rs (the parity test enforces it).
 */

export function listTypesTool(vaultDir: string): Tool {
  return {
    name: 'list_types',
    description:
      "List declared note types (.tacitus/types/*.yaml): each property's type (string|number|boolean|date|list), required, enum. Creates/updates of a note whose frontmatter `type` names a declared type are validated; violations return SCHEMA_VIOLATION listing every problem.",
    inputSchema: z.object({}),
    handler: async () => {
      const entries = await new TypeStore(vaultDir).list();
      return {
        types: entries.map((e) =>
          'error' in e ? { name: e.name, error: e.error } : typeToJson(e),
        ),
      };
    },
  };
}

/**
 * Body + frontmatter of a fresh daily note: the `daily` template when it needs
 * no vars beyond {{date}} (filled with the capture's date, not today), else
 * `# <date>` with `type: daily` and `date`.
 */
async function newDaily(
  vaultDir: string,
  noteId: string,
  date: string,
): Promise<{ content: string; frontmatter?: Record<string, unknown> }> {
  try {
    const raw = await new TemplateStore(vaultDir).loadRaw(DAILY_TEMPLATE);
    const note = parseNote(renderTemplate(raw, { date }), `${noteId}.md`);
    const fm = Object.keys(note.frontmatter).length > 0 ? note.frontmatter : undefined;
    return { content: note.content, frontmatter: fm };
  } catch {
    // No template, or it needs vars a capture can't supply.
    return { content: `# ${date}\n`, frontmatter: { type: 'daily', date } };
  }
}

export function captureTool(vaultDir: string, writer: NoteWriter): Tool {
  // Captures run one at a time so two can't both append to the same stale content.
  let queue: Promise<unknown> = Promise.resolve();
  return {
    name: 'capture',
    description:
      'Quick capture: append a line to a daily note (daily/YYYY-MM-DD, default today; created on first capture, from the `daily` template if one exists). task=true appends a `- [ ]` checklist item that list_tasks sees; otherwise a timestamped bullet. Versioned + audited. Returns note_id, version_id and the line written.',
    inputSchema: z.object({
      text: z.string(),
      task: z.boolean().optional(),
      date: z.string().optional(),
    }),
    handler: (input) => {
      const run = async (): Promise<Record<string, unknown>> => {
        const now = new Date();
        const date = input.date ?? builtinValue('date', now);
        const noteId = dailyNoteId(date);
        const line = captureLine(input.text, input.task ?? false, builtinValue('time', now));
        const existing = await writer.readNote(noteId);
        if (existing) {
          const { version_id } = await writer.updateNote(noteId, {
            content: appendLine(existing.content, line),
          });
          return { note_id: noteId, version_id, line, created: false };
        }
        const fresh = await newDaily(vaultDir, noteId, date);
        const { version_id } = await writer.createNote(
          noteId,
          appendLine(fresh.content, line),
          fresh.frontmatter,
        );
        return { note_id: noteId, version_id, line, created: true };
      };
      const result = queue.then(run, run);
      queue = result.catch(() => undefined);
      return result;
    },
  };
}

export function typeAndCaptureTools(vaultDir: string, writer: NoteWriter): Tool[] {
  return [listTypesTool(vaultDir), captureTool(vaultDir, writer)];
}
