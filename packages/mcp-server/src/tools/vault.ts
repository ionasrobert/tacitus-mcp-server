import { z } from 'zod';
import type { Embedder } from '../vault/embed';
import { getNote } from '../vault/get';
import { graphQuery } from '../vault/graph';
import type { VaultIndex } from '../vault/index';
import { searchNotes } from '../vault/search';
import type { Tool } from './types';

export function searchTool(index: VaultIndex, embedder?: Embedder): Tool {
  return {
    name: 'search',
    description:
      'Search vault notes by relevance; returns ranked snippets within an optional token_budget (never whole notes). mode: hybrid (default) | lexical | semantic. Expand with get_note.',
    inputSchema: z.object({
      query: z.string(),
      mode: z.enum(['lexical', 'semantic', 'hybrid']).optional(),
      token_budget: z.number().int().positive().optional(),
      top_k: z.number().int().positive().optional(),
    }),
    handler: (input) =>
      searchNotes(index, input.query, {
        mode: input.mode,
        token_budget: input.token_budget,
        top_k: input.top_k,
        embedder,
      }),
  };
}

export function getNoteTool(index: VaultIndex): Tool {
  return {
    name: 'get_note',
    description:
      'Fetch a note progressively: outline (headings) | frontmatter_only | full, with an optional max_tokens ceiling.',
    inputSchema: z.object({
      note_id: z.string(),
      format: z.enum(['outline', 'frontmatter_only', 'full']).optional(),
      max_tokens: z.number().int().positive().optional(),
    }),
    handler: (input) =>
      getNote(index, input.note_id, { format: input.format, max_tokens: input.max_tokens }),
  };
}

export function graphQueryTool(index: VaultIndex): Tool {
  return {
    name: 'graph_query',
    description:
      'Traverse the wikilink graph: links (outgoing) | backlinks | neighbors (both directions, to depth).',
    inputSchema: z.object({
      from: z.string(),
      relation: z.enum(['links', 'backlinks', 'neighbors']),
      depth: z.number().int().positive().optional(),
    }),
    handler: (input) =>
      graphQuery(index, { from: input.from, relation: input.relation, depth: input.depth }),
  };
}

export function listNotesTool(index: VaultIndex): Tool {
  return {
    name: 'list_notes',
    description: 'List all note ids with their titles and paths.',
    inputSchema: z.object({}).strict(),
    handler: () => ({
      notes: index.all().map((n) => ({ note_id: n.id, title: n.title, path: n.path })),
    }),
  };
}

export function vaultTools(index: VaultIndex, embedder?: Embedder): Tool[] {
  return [
    searchTool(index, embedder),
    getNoteTool(index),
    graphQueryTool(index),
    listNotesTool(index),
  ];
}
