import { z } from 'zod';
import { TacitusError } from '../lib/errors';
import type { Embedder } from '../vault/embed';
import type { VaultIndex } from '../vault/index';
import { parseNote } from '../vault/parse';
import { PROP_OPS, propertiesQuery, type PropFilter, type PropOp } from '../vault/properties';
import { renameNoteOps } from '../vault/rename';
import { suggestLinks } from '../vault/suggest';
import { listTasks, toggledContent } from '../vault/tasks';
import { renderTemplate, TemplateStore } from '../vault/template';
import type { NoteWriter } from '../vault/write';
import type { Tool } from './types';

/**
 * The tools that used to be native-Rust-only: link suggestions, properties,
 * templates, tasks, rename/delete and version inspection. Descriptions are
 * byte-identical to crates/tacitus-core/src/tools/mod.rs so both engines
 * advertise the same contract.
 */

const count = z.number().int().nonnegative();

export function suggestLinksTool(index: VaultIndex, embedder?: Embedder): Tool {
  return {
    name: 'suggest_links',
    description:
      "Suggest [[wikilinks]] a note is missing: ranked candidates it doesn't link to yet, scored by title mentions in the note, semantic similarity, shared tags, and existing backlinks — each with machine-readable reasons. Bounded by top_k/min_score/token_budget.",
    inputSchema: z.object({
      note_id: z.string(),
      top_k: count.optional(),
      min_score: z.number().optional(),
      token_budget: count.optional(),
    }),
    handler: async (input) => ({
      note_id: input.note_id,
      suggestions: await suggestLinks(index, input.note_id, embedder, {
        top_k: input.top_k,
        min_score: input.min_score,
        token_budget: input.token_budget,
      }),
    }),
  };
}

export function propertiesQueryTool(index: VaultIndex): Tool {
  return {
    name: 'properties_query',
    description:
      'Query notes by typed frontmatter properties (Bases-like). filters are AND-ed {key, op: eq|ne|contains|exists|not_exists|gt|lt|gte|lte, value}; contains = array membership or substring; gt/lt work on numbers and ISO dates. Optional select (project keys), sort_by/descending, limit (default 50), token_budget.',
    inputSchema: z.object({
      filters: z
        .object({ key: z.string(), op: z.string(), value: z.unknown().optional() })
        .array()
        .optional(),
      select: z.string().array().optional(),
      sort_by: z.string().optional(),
      descending: z.boolean().optional(),
      limit: count.optional(),
      token_budget: count.optional(),
    }),
    handler: (input) => {
      const filters: PropFilter[] = (input.filters ?? []).map(
        (f: { key: string; op: string; value?: unknown }) => {
          if (!(PROP_OPS as readonly string[]).includes(f.op)) {
            throw new TacitusError({
              code: 'INVALID_INPUT',
              reason: `op must be eq|ne|contains|exists|not_exists|gt|lt|gte|lte (got ${JSON.stringify(f.op)}).`,
              suggestion: 'Use a valid op.',
            });
          }
          return { key: f.key, op: f.op as PropOp, value: f.value };
        },
      );
      return {
        rows: propertiesQuery(index, {
          filters,
          select: input.select,
          sort_by: input.sort_by,
          descending: input.descending ?? false,
          limit: input.limit,
          token_budget: input.token_budget,
        }),
      };
    },
  };
}

export function listTemplatesTool(vaultDir: string): Tool {
  return {
    name: 'list_templates',
    description:
      'List note templates (.tacitus/templates/*.md) with the vars each requires. Builtins {{date}}/{{time}}/{{datetime}} auto-fill.',
    inputSchema: z.object({}).strict(),
    handler: async () => ({ templates: await new TemplateStore(vaultDir).list() }),
  };
}

export function createFromTemplateTool(vaultDir: string, writer: NoteWriter): Tool {
  return {
    name: 'create_from_template',
    description:
      'Create a note from a template (auto-committed, versioned, audited). Substitution happens before YAML parsing, so numeric vars stay typed. Returns a version_id.',
    inputSchema: z.object({
      template: z.string(),
      note_id: z.string(),
      vars: z.record(z.unknown()).optional(),
    }),
    handler: async (input) => {
      const raw = await new TemplateStore(vaultDir).loadRaw(input.template);
      const vars: Record<string, string> = {};
      for (const [k, v] of Object.entries((input.vars ?? {}) as Record<string, unknown>)) {
        if (typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean') {
          vars[k] = String(v);
        } else {
          throw new TacitusError({
            code: 'INVALID_INPUT',
            reason: `var ${JSON.stringify(k)} must be a scalar (string/number/bool).`,
            suggestion: 'Pass scalar values in vars.',
          });
        }
      }
      const note = parseNote(renderTemplate(raw, vars), `${input.note_id}.md`);
      const fm = Object.keys(note.frontmatter).length > 0 ? note.frontmatter : undefined;
      const { version_id } = await writer.createNote(input.note_id, note.content, fm);
      return { version_id, note_id: input.note_id };
    },
  };
}

export function listTasksTool(index: VaultIndex): Tool {
  return {
    name: 'list_tasks',
    description:
      'List tasks (checklist lines) across the vault as typed entities: text, done, due (from due:YYYY-MM-DD or 📅 YYYY-MM-DD), #tags. Filter by done/due_before/due_after/tag/note_id; sorted by due date; bounded by limit + token_budget.',
    inputSchema: z.object({
      done: z.boolean().optional(),
      due_before: z.string().optional(),
      due_after: z.string().optional(),
      tag: z.string().optional(),
      note_id: z.string().optional(),
      limit: count.optional(),
      token_budget: count.optional(),
    }),
    handler: (input) => ({ tasks: listTasks(index, input) }),
  };
}

export function toggleTaskTool(writer: NoteWriter): Tool {
  return {
    name: 'toggle_task',
    description:
      "Flip a task's checkbox (versioned + audited). Pass the line and text exactly as returned by list_tasks — if the line changed since, you get a CONFLICT instead of toggling the wrong task.",
    inputSchema: z.object({ note_id: z.string(), line: count, expect_text: z.string() }),
    handler: async (input) => {
      // Read from disk, not the index: the guard must see the note as it is now.
      const note = await writer.readNote(input.note_id);
      if (!note) {
        throw new TacitusError({
          code: 'NOTE_NOT_FOUND',
          reason: `No note with id ${JSON.stringify(input.note_id)}.`,
          suggestion: 'Use list_tasks to discover tasks.',
        });
      }
      const content = toggledContent(input.note_id, note.content, input.line, input.expect_text);
      return writer.updateNote(input.note_id, { content });
    },
  };
}

export function renameNoteTool(index: VaultIndex, writer: NoteWriter): Tool {
  return {
    name: 'rename_note',
    description:
      'Rename a note AND retarget every wikilink that resolves to it (alias/heading kept) — one atomic, versioned changeset; a single revert undoes the whole rename.',
    inputSchema: z.object({ from: z.string(), to: z.string() }),
    handler: async (input) => {
      const changeset = renameNoteOps(index, input.from, input.to);
      const { version_id } = await writer.apply(changeset);
      return {
        version_id,
        from: input.from,
        to: input.to,
        links_updated_in: Math.max(0, changeset.ops.length - 2), // minus create+delete
      };
    },
  };
}

export function deleteNoteTool(writer: NoteWriter): Tool {
  return {
    name: 'delete_note',
    description: 'Delete a note (versioned + audited — revert restores it).',
    inputSchema: z.object({ note_id: z.string() }),
    handler: (input) => writer.deleteNote(input.note_id),
  };
}

export function getVersionTool(writer: NoteWriter): Tool {
  return {
    name: 'get_version',
    description:
      "Inspect a committed version: which notes it created/updated/deleted (default), plus each note's before/after when include_content=true (truncated to max_tokens each, default 500). Pairs with audit_log and revert.",
    inputSchema: z.object({
      version_id: z.string(),
      include_content: z.boolean().optional(),
      max_tokens: count.optional(),
    }),
    handler: async (input) => {
      const detail = await writer.getVersion(input.version_id);
      const include = input.include_content ?? false;
      const maxChars = (input.max_tokens ?? 500) * 4;
      // Clip by code points, like Rust's chars().
      const clip = (text: string | null): { content: string; truncated: boolean } | null => {
        if (text === null || !include) return null;
        const chars = Array.from(text);
        return { content: chars.slice(0, maxChars).join(''), truncated: chars.length > maxChars };
      };
      return {
        version_id: detail.version_id,
        change_id: detail.change_id,
        notes: detail.notes.map((n) => ({
          note_id: n.note_id,
          op: n.op,
          before: clip(n.before),
          after: clip(n.after),
        })),
      };
    },
  };
}

/** Read-side structured tools (index-backed). */
export function structuredReadTools(
  index: VaultIndex,
  vaultDir: string,
  embedder?: Embedder,
): Tool[] {
  return [
    suggestLinksTool(index, embedder),
    propertiesQueryTool(index),
    listTemplatesTool(vaultDir),
    listTasksTool(index),
  ];
}

/** Write-side structured tools (all commit through NoteWriter), plus get_version. */
export function structuredWriteTools(
  index: VaultIndex,
  vaultDir: string,
  writer: NoteWriter,
): Tool[] {
  return [
    createFromTemplateTool(vaultDir, writer),
    toggleTaskTool(writer),
    renameNoteTool(index, writer),
    deleteNoteTool(writer),
    getVersionTool(writer),
  ];
}
