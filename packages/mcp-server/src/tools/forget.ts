import { z } from 'zod';
import type { MemoryStore } from '../memory/store';
import type { Tool } from './types';

const ForgetInput = z.object({ memory_id: z.string() });

export function forgetTool(store: MemoryStore): Tool {
  return {
    name: 'forget',
    description: 'Delete a memory by id. Returns whether a memory was removed.',
    inputSchema: ForgetInput,
    handler: async (input) => ({ removed: await store.remove(input.memory_id) }),
  };
}
