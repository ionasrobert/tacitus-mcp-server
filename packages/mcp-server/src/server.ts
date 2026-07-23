import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { join } from 'node:path';
import type { z } from 'zod';
import { MemoryStore } from './memory/store';
import { capabilitiesTool } from './tools/capabilities';
import { memoryTools } from './tools';
import { runTool, type PermissionScope, type Tool } from './tools/types';
import { vaultTools } from './tools/vault';
import { writeTools } from './tools/write';
import { convenienceTools } from './tools/convenience';
import { HashingEmbedder, type Embedder } from './vault/embed';
import { CachedEmbedder } from './vault/embed-cache';
import { TransformersEmbedder } from './vault/transformers';
import { VaultIndex } from './vault/index';
import { NoteWriter } from './vault/write';

/**
 * Choose the embedder for semantic search. Default is the deterministic,
 * offline HashingEmbedder. Set TACITUS_EMBEDDER=transformers to use a neural
 * model (requires `npm i @huggingface/transformers`); it falls back to hashing
 * if the optional dependency or model is unavailable.
 */
async function pickEmbedder(vaultDir: string): Promise<Embedder> {
  if (process.env.TACITUS_EMBEDDER === 'transformers') {
    try {
      const neural = new TransformersEmbedder();
      await neural.embedBatch(['warmup']); // surface load failures at startup
      process.stderr.write('tacitus: using transformers.js neural embedder\n');
      return new CachedEmbedder(neural, join(vaultDir, '.tacitus', 'vectors', 'neural.json'));
    } catch (err) {
      const reason = err instanceof Error ? err.message : String(err);
      process.stderr.write(
        `tacitus: neural embedder unavailable (${reason}); falling back to hashing\n`,
      );
    }
  }
  return new HashingEmbedder();
}

/** Wire the full Tacitus tool set (memory + vault) into an MCP server. */
export async function buildServer(
  vaultDir: string,
  scope: PermissionScope = 'read-write',
): Promise<McpServer> {
  const store = new MemoryStore(vaultDir);
  const index = await VaultIndex.build(vaultDir);
  const writer = new NoteWriter(vaultDir, scope, index);
  const embedder = await pickEmbedder(vaultDir);

  const tools: Tool[] = [
    ...memoryTools(store),
    ...vaultTools(index, embedder),
    ...writeTools(writer),
    ...convenienceTools(writer),
  ];
  tools.push(capabilitiesTool(() => tools, scope));

  const server = new McpServer({ name: 'tacitus-memory', version: '0.1.0' });
  for (const tool of tools) {
    const shape = (tool.inputSchema as z.ZodObject<z.ZodRawShape>).shape;
    server.tool(tool.name, tool.description, shape, async (args: Record<string, unknown>) => {
      const result = await runTool(tool, args);
      return {
        content: [{ type: 'text' as const, text: JSON.stringify(result, null, 2) }],
        isError: !result.ok,
      };
    });
  }

  return server;
}
