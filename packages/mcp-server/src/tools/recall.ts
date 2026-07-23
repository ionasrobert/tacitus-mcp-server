import { z } from 'zod';
import { recall } from '../memory/recall';
import type { MemoryStore } from '../memory/store';
import type { Tool } from './types';

const RecallInput = z.object({
  query: z.string(),
  type: z.enum(['user', 'feedback', 'project', 'reference']).optional(),
  token_budget: z.number().int().positive().optional(),
});

export function recallTool(store: MemoryStore): Tool {
  return {
    name: 'recall',
    description:
      'Recall memories relevant to a query, ranked, within an optional token_budget. Surfaces conflicting memories instead of silently choosing.',
    inputSchema: RecallInput,
    handler: async (input) => {
      const memories = await store.load();
      return recall(memories, input);
    },
  };
}
