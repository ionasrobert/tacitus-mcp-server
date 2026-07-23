import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createTools } from '../../src/tools';
import { runTool, type Tool } from '../../src/tools/types';
import { MemoryStore } from '../../src/memory/store';

describe('MCP tool contract (M5)', () => {
  let dir: string;
  let byName: Record<string, Tool>;

  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), 'tacitus-mcp-'));
    const tools = createTools(new MemoryStore(dir));
    byName = Object.fromEntries(tools.map((t) => [t.name, t]));
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('exposes exactly the expected tools', () => {
    expect(Object.keys(byName).sort()).toEqual(['capabilities', 'forget', 'recall', 'remember']);
  });

  it('remember: valid input returns a typed memory_id', async () => {
    const res = await runTool(byName.remember!, {
      content: 'contract test fact',
      type: 'user',
      source: { origin: 'contract-test', author: 'agent' },
    });
    expect(res.ok).toBe(true);
    if (res.ok) expect(typeof (res.data as { memory_id: string }).memory_id).toBe('string');
  });

  it('remember: missing provenance returns a structured error, never throws', async () => {
    const res = await runTool(byName.remember!, { content: 'x', type: 'user' });
    expect(res.ok).toBe(false);
    if (!res.ok) {
      expect(res.error.code).toBe('MISSING_PROVENANCE');
      expect(res.error.suggestion).toBeTruthy();
    }
  });

  it('remember: malformed input returns INVALID_INPUT', async () => {
    const res = await runTool(byName.remember!, { type: 'user' }); // missing content
    expect(res.ok).toBe(false);
    if (!res.ok) expect(res.error.code).toBe('INVALID_INPUT');
  });

  it('recall: returns items and conflicts arrays after a remember', async () => {
    await runTool(byName.remember!, {
      content: 'the launch date is in March',
      type: 'project',
      source: { origin: 'contract-test', author: 'agent' },
    });
    const res = await runTool(byName.recall!, { query: 'launch date', token_budget: 100 });
    expect(res.ok).toBe(true);
    if (res.ok) {
      const data = res.data as { items: unknown[]; conflicts: unknown[] };
      expect(Array.isArray(data.items)).toBe(true);
      expect(Array.isArray(data.conflicts)).toBe(true);
      expect(data.items.length).toBeGreaterThan(0);
    }
  });

  it('capabilities: lists tools and the current permission scope', async () => {
    const res = await runTool(byName.capabilities!, {});
    expect(res.ok).toBe(true);
    if (res.ok) {
      const caps = res.data as { tools: { name: string }[]; permissions: { scope: string } };
      expect(caps.tools.map((t) => t.name).sort()).toContain('remember');
      expect(caps.permissions.scope).toBe('read-write');
    }
  });
});
