import { z } from 'zod';
import { remember } from '../memory/remember';
import type { MemoryStore } from '../memory/store';
import type { Tool } from './types';

/**
 * The schema stays lenient on `type` and `source` so that provenance/type
 * enforcement happens in the core remember() — yielding the richer
 * MISSING_PROVENANCE / INVALID_TYPE errors instead of a generic schema error.
 */
const RememberInput = z.object({
  content: z.string(),
  type: z.string(),
  tags: z.array(z.string()).optional(),
  key: z.string().optional(),
  source: z
    .object({
      origin: z.string(),
      author: z.enum(['human', 'agent']),
      timestamp: z.string().optional(),
    })
    .optional(),
  ttl: z.number().optional(),
});

export function rememberTool(store: MemoryStore): Tool {
  return {
    name: 'remember',
    description:
      'Store a typed memory (user|feedback|project|reference) with mandatory provenance. Returns a stable memory_id; idempotent for identical content+source.',
    inputSchema: RememberInput,
    handler: async (input) => {
      const memory = remember(input);
      await store.save(memory);
      return { memory_id: memory.id };
    },
  };
}
