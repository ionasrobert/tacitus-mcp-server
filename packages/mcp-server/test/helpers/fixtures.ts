import type { Memory } from '../../src/memory/types';

let counter = 0;

interface MemoryOverrides {
  id?: string;
  content?: string;
  type?: Memory['type'];
  tags?: string[];
  key?: string;
  ttl?: number;
  source?: Memory['source'];
}

/** Build a fully-formed stored Memory for tests that don't exercise remember(). */
export function makeMemory(overrides: MemoryOverrides = {}): Memory {
  counter += 1;
  const memory: Memory = {
    id: overrides.id ?? `mem_test_${counter}`,
    content: overrides.content ?? `fact number ${counter}`,
    type: overrides.type ?? 'user',
    tags: overrides.tags ?? [],
    source: overrides.source ?? {
      origin: 'test',
      author: 'agent',
      timestamp: '2026-07-23T10:00:00.000Z',
    },
  };
  if (overrides.key !== undefined) memory.key = overrides.key;
  if (overrides.ttl !== undefined) memory.ttl = overrides.ttl;
  return memory;
}
