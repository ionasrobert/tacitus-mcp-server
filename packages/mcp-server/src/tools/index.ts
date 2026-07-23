import type { MemoryStore } from '../memory/store';
import { capabilitiesTool } from './capabilities';
import { forgetTool } from './forget';
import { recallTool } from './recall';
import { rememberTool } from './remember';
import type { PermissionScope, Tool } from './types';

/** The memory tool set (no capabilities tool). */
export function memoryTools(store: MemoryStore): Tool[] {
  return [rememberTool(store), recallTool(store), forgetTool(store)];
}

/** Memory tools + a capabilities tool over them (used by the memory contract). */
export function createTools(store: MemoryStore, scope: PermissionScope = 'read-write'): Tool[] {
  const tools = memoryTools(store);
  tools.push(capabilitiesTool(() => tools, scope));
  return tools;
}
