import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { readFile, rm, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { buildServer } from '../../src/server';
import { makeVault } from '../helpers/vault';

/** Wire-level behavior over a real MCP client (what an agent actually sees). */
describe('npm server over the wire', () => {
  let dir: string;
  let client: Client;
  const call = async (name: string, args: Record<string, unknown> = {}) => {
    const res = await client.callTool({ name, arguments: args });
    return JSON.parse((res.content as { text: string }[])[0]!.text) as {
      ok: boolean;
      data: any;
      error?: { code: string };
    };
  };

  beforeEach(async () => {
    dir = await makeVault();
    const server = await buildServer(dir);
    const [a, b] = InMemoryTransport.createLinkedPair();
    client = new Client({ name: 'wire-test', version: '0' });
    await Promise.all([server.connect(a), client.connect(b)]);
  });
  afterEach(async () => {
    await client.close();
    await rm(dir, { recursive: true, force: true });
  });

  it('sees notes edited outside Tacitus without a restart', async () => {
    const before = await call('list_notes');
    expect(before.data.notes.map((n: { note_id: string }) => n.note_id)).not.toContain('outside');
    await writeFile(join(dir, 'outside.md'), '# Outside\n\nWritten by Obsidian. #fresh\n');
    const after = await call('list_notes');
    expect(after.data.notes.map((n: { note_id: string }) => n.note_id)).toContain('outside');
    const hits = await call('search', { query: 'written by obsidian', mode: 'lexical' });
    expect(hits.data.hits[0].note_id).toBe('outside');
  });

  it('capabilities reports the package version, not a literal', async () => {
    const pkg = JSON.parse(await readFile(new URL('../../package.json', import.meta.url), 'utf8'));
    const caps = await call('capabilities');
    expect(caps.data.version).toBe(pkg.version);
    expect(client.getServerVersion()?.version).toBe(pkg.version);
  });

  it('recall conflicts use the documented memory_ids field', async () => {
    const source = { origin: 'test', author: 'human' };
    await call('remember', {
      content: 'Deploys on Tuesday.',
      type: 'project',
      key: 'deploy-day',
      source,
    });
    await call('remember', {
      content: 'Deploys on Friday.',
      type: 'project',
      key: 'deploy-day',
      source,
    });
    const res = await call('recall', { query: 'deploys' });
    expect(res.data.conflicts).toHaveLength(1);
    expect(res.data.conflicts[0].memory_ids).toHaveLength(2);
    expect(res.data.conflicts[0]).not.toHaveProperty('memoryIds');
  });
});
