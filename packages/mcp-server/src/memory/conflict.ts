import type { Memory } from './types';

export interface Conflict {
  key: string;
  memoryIds: string[];
}

/**
 * Detect contradicting memories: same `key`, different `content`.
 * We surface the conflict rather than silently choosing (Part I.2).
 * Memories without a key are ignored.
 */
export function detectConflicts(memories: Memory[]): Conflict[] {
  const byKey = new Map<string, Memory[]>();
  for (const memory of memories) {
    if (memory.key === undefined) continue;
    const group = byKey.get(memory.key) ?? [];
    group.push(memory);
    byKey.set(memory.key, group);
  }

  const conflicts: Conflict[] = [];
  for (const [key, group] of byKey) {
    const distinctContents = new Set(group.map((m) => m.content));
    if (distinctContents.size > 1) {
      conflicts.push({ key, memoryIds: group.map((m) => m.id) });
    }
  }
  return conflicts;
}
