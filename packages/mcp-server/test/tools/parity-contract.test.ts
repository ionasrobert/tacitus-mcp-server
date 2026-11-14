import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { readFile, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import { buildServer } from '../../src/server';
import { makeVault } from '../helpers/vault';

/**
 * Anti-drift: the npm server must publish exactly the native engine's tool
 * set, with byte-identical descriptions. The source of truth is the TOOLS
 * table in crates/tacitus-core/src/tools/mod.rs.
 */
async function rustTools(): Promise<Map<string, string>> {
  const path = fileURLToPath(
    new URL('../../../../crates/tacitus-core/src/tools/mod.rs', import.meta.url),
  );
  const src = await readFile(path, 'utf8');
  const re =
    /ToolDescriptor \{\s*name: "([a-z_]+)",\s*writes: (?:true|false),\s*description: "((?:[^"\\]|\\.)*)",/g;
  const out = new Map<string, string>();
  for (const m of src.matchAll(re)) {
    out.set(m[1]!, m[2]!.replace(/\\(["\\])/g, '$1'));
  }
  return out;
}

describe('npm ↔ native tool parity', () => {
  let dir: string;
  let client: Client;
  beforeEach(async () => {
    dir = await makeVault();
    const server = await buildServer(dir);
    const [a, b] = InMemoryTransport.createLinkedPair();
    client = new Client({ name: 'parity-test', version: '0' });
    await Promise.all([server.connect(a), client.connect(b)]);
  });
  afterEach(async () => {
    await client.close();
    await rm(dir, { recursive: true, force: true });
  });

  it('publishes the same tools as the Rust engine, with identical descriptions', async () => {
    const native = await rustTools();
    expect(native.size).toBeGreaterThanOrEqual(25); // the parser still sees the table
    const { tools } = await client.listTools();
    const published = new Map(tools.map((t) => [t.name, t.description ?? '']));
    expect([...published.keys()].sort()).toEqual([...native.keys()].sort());
    for (const [name, description] of native) {
      expect({ name, description: published.get(name) }).toEqual({ name, description });
    }
  });

  it('a new tool round-trips over the wire (list_tasks)', async () => {
    const res = await client.callTool({ name: 'list_tasks', arguments: {} });
    expect(res.isError).toBe(false);
  });
});
