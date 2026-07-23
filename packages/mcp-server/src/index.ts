#!/usr/bin/env node
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { buildServer } from './server';

const vaultDir = process.argv[2] ?? process.cwd();

async function main(): Promise<void> {
  const server = await buildServer(vaultDir);
  const transport = new StdioServerTransport();
  await server.connect(transport);
  // stdout is the protocol channel on stdio transport — log to stderr only.
  process.stderr.write(`tacitus-memory MCP server running on stdio (vault: ${vaultDir})\n`);
}

main().catch((err: unknown) => {
  process.stderr.write(`Fatal: ${err instanceof Error ? err.message : String(err)}\n`);
  process.exit(1);
});
