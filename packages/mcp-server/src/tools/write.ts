import { z } from 'zod';
import type { Changeset, NoteWriter } from '../vault/write';
import type { Tool } from './types';

const CreateOp = z.object({
  op: z.literal('create'),
  note_id: z.string(),
  content: z.string(),
  frontmatter: z.record(z.unknown()).optional(),
});
const UpdateOp = z.object({
  op: z.literal('update'),
  note_id: z.string(),
  content: z.string().optional(),
  frontmatter: z.record(z.unknown()).optional(),
});
const DeleteOp = z.object({
  op: z.literal('delete'),
  note_id: z.string(),
});
const ChangesetSchema = z.object({
  ops: z.discriminatedUnion('op', [CreateOp, UpdateOp, DeleteOp]).array().min(1),
});

export function proposeChangesTool(writer: NoteWriter): Tool {
  return {
    name: 'propose_changes',
    description:
      'Dry-run a changeset (create/update/delete notes). Returns a change_id and a before/after diff. Nothing is written until commit_changes.',
    inputSchema: ChangesetSchema,
    handler: (input) => writer.propose(input as Changeset),
  };
}

export function commitChangesTool(writer: NoteWriter): Tool {
  return {
    name: 'commit_changes',
    description:
      'Atomically apply a previously proposed change_id (all-or-nothing). Returns a version_id usable with revert.',
    inputSchema: z.object({ change_id: z.string() }),
    handler: (input) => writer.commit(input.change_id),
  };
}

export function revertTool(writer: NoteWriter): Tool {
  return {
    name: 'revert',
    description: 'Undo a committed change by version_id, restoring the prior note state.',
    inputSchema: z.object({ version_id: z.string() }),
    handler: (input) => writer.revert(input.version_id),
  };
}

export function writeTools(writer: NoteWriter): Tool[] {
  return [proposeChangesTool(writer), commitChangesTool(writer), revertTool(writer)];
}
