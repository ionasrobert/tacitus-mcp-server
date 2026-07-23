import { z } from 'zod';
import type { PermissionScope, Tool } from './types';

export interface Capabilities {
  server: string;
  version: string;
  tools: { name: string; description: string }[];
  permissions: { scope: PermissionScope };
}

/**
 * Tells the agent what it can do and under what permission scope (Part I.5) —
 * so it never has to guess.
 */
export function capabilitiesTool(listTools: () => Tool[], scope: PermissionScope): Tool {
  return {
    name: 'capabilities',
    description: 'List available tools and the current permission scope.',
    inputSchema: z.object({}).strict(),
    handler: (): Capabilities => ({
      server: 'tacitus-memory',
      version: '0.1.0',
      tools: listTools().map((t) => ({ name: t.name, description: t.description })),
      permissions: { scope },
    }),
  };
}
