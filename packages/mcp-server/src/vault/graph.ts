import { TacitusError } from '../lib/errors';
import type { VaultIndex } from './index';

export type Relation = 'links' | 'backlinks' | 'neighbors';

export interface GraphArgs {
  from: string;
  relation: Relation;
  /** BFS depth for `neighbors` (default 1). */
  depth?: number;
}

export interface GraphNode {
  note_id: string;
  title: string;
}

export interface GraphResult {
  from: string;
  relation: Relation;
  nodes: GraphNode[];
}

/**
 * The link graph as a queryable API (Part I.3): outgoing links, backlinks, or
 * neighbors (both directions, BFS to `depth`). Agents traverse instead of grep.
 */
export function graphQuery(index: VaultIndex, args: GraphArgs): GraphResult {
  if (!index.get(args.from)) {
    throw new TacitusError({
      code: 'NOTE_NOT_FOUND',
      reason: `No note with id "${args.from}".`,
      suggestion: 'Use list_notes to discover valid note ids.',
    });
  }

  let ids: string[];
  if (args.relation === 'links') {
    ids = index.outgoing(args.from).map((n) => n.id);
  } else if (args.relation === 'backlinks') {
    ids = index.backlinks(args.from).map((n) => n.id);
  } else {
    ids = neighbors(index, args.from, args.depth ?? 1);
  }

  const nodes: GraphNode[] = [];
  for (const id of ids) {
    const note = index.get(id);
    if (note) nodes.push({ note_id: note.id, title: note.title });
  }
  return { from: args.from, relation: args.relation, nodes };
}

function neighbors(index: VaultIndex, from: string, depth: number): string[] {
  const seen = new Set<string>([from]);
  let frontier = [from];
  const collected = new Set<string>();
  for (let d = 0; d < depth; d += 1) {
    const next: string[] = [];
    for (const id of frontier) {
      const adjacent = [...index.outgoing(id), ...index.backlinks(id)];
      for (const note of adjacent) {
        if (!seen.has(note.id)) {
          seen.add(note.id);
          collected.add(note.id);
          next.push(note.id);
        }
      }
    }
    frontier = next;
  }
  return [...collected];
}
