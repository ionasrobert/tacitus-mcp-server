import { z } from 'zod';
import type { NoteWriter } from '../vault/write';
import type { Tool } from './types';

/**
 * Ergonomic single-note operations that auto-commit through NoteWriter — so
 * they remain transactional, versioned (revertable), permission-scoped, and
 * audited, unlike a raw filesystem write.
 */
export function createNoteTool(writer: NoteWriter): Tool {
  return {
    name: 'create_note',
    description: 'Create a note (auto-committed). Fails if it already exists. Returns a version_id.',
    inputSchema: z.object({
      note_id: z.string(),
      content: z.string(),
      frontmatter: z.record(z.unknown()).optional(),
    }),
    handler: (input) => writer.createNote(input.note_id, input.content, input.frontmatter),
  };
}

export function updateNoteTool(writer: NoteWriter): Tool {
  return {
    name: 'update_note',
    description: 'Update a note body and/or frontmatter (auto-committed). Returns a version_id.',
    inputSchema: z.object({
      note_id: z.string(),
      content: z.string().optional(),
      frontmatter: z.record(z.unknown()).optional(),
    }),
    handler: (input) =>
      writer.updateNote(input.note_id, { content: input.content, frontmatter: input.frontmatter }),
  };
}

export function linkTool(writer: NoteWriter): Tool {
  return {
    name: 'link',
    description: 'Append a [[to]] wikilink to the `from` note (idempotent). Returns a version_id.',
    inputSchema: z.object({ from: z.string(), to: z.string() }),
    handler: (input) => writer.link(input.from, input.to),
  };
}

export function tagTool(writer: NoteWriter): Tool {
  return {
    name: 'tag',
    description: 'Add a tag to a note (deduplicated). Returns a version_id.',
    inputSchema: z.object({ note_id: z.string(), tag: z.string() }),
    handler: (input) => writer.tag(input.note_id, input.tag),
  };
}

export function auditLogTool(writer: NoteWriter): Tool {
  return {
    name: 'audit_log',
    description: 'Read recent agent actions (commits/reverts), most recent first.',
    inputSchema: z.object({ limit: z.number().int().positive().optional() }),
    handler: async (input) => ({ entries: await writer.readAudit(input.limit) }),
  };
}

export function convenienceTools(writer: NoteWriter): Tool[] {
  return [
    createNoteTool(writer),
    updateNoteTool(writer),
    linkTool(writer),
    tagTool(writer),
    auditLogTool(writer),
  ];
}
