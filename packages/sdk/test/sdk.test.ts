// E2E over real stdio: the SDK spawns the reference npm server (via tsx on
// source — CI runs tests before build) against a temp vault and exercises the
// typed surface, including the structured-error mapping that IS the product.

import { mkdtemp, rm, mkdir, writeFile, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import { TacitusClient, TacitusToolError } from '../src/index.js';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '../../..');
const tsxBin = join(repoRoot, 'node_modules', '.bin', 'tsx');
const serverSrc = join(repoRoot, 'packages', 'mcp-server', 'src', 'index.ts');

let vault: string;
let tacitus: TacitusClient;

beforeAll(async () => {
  vault = await mkdtemp(join(tmpdir(), 'tacitus-sdk-'));
  await mkdir(join(vault, 'notes'), { recursive: true });
  await writeFile(
    join(vault, 'notes', 'alpha.md'),
    '# Alpha\n\nLaunch checklist for the alpha release.\n',
  );
  tacitus = await TacitusClient.spawn({ vault, command: tsxBin, args: [serverSrc] });
}, 30_000);

afterAll(async () => {
  await tacitus?.close();
  await rm(vault, { recursive: true, force: true });
});

describe('TacitusClient over stdio', () => {
  it('capabilities lists the npm server surface', async () => {
    const caps = await tacitus.capabilities();
    expect(caps.tools.length).toBe(16);
    expect(caps.permissions.scope).toBe('read-write');
    expect(caps.tools.map((t) => t.name)).toContain('search');
  });

  it('remember → recall roundtrip, fully typed', async () => {
    const { memory_id } = await tacitus.remember({
      content: 'The plugin marketplace launches in autumn.',
      type: 'project',
      source: { origin: 'sdk-test', author: 'agent' },
    });
    expect(memory_id).toMatch(/^mem_/);

    const result = await tacitus.recall({ query: 'marketplace launch' });
    expect(result.items.length).toBeGreaterThan(0);
    expect(result.items[0]!.memory.content).toContain('marketplace');
    expect(result.conflicts).toEqual([]);
  });

  it('maps structured errors to TacitusToolError (the contract in action)', async () => {
    const bare = { content: 'no provenance', type: 'project' };
    const promise = tacitus.remember(bare as never);
    await expect(promise).rejects.toBeInstanceOf(TacitusToolError);
    await expect(promise).rejects.toMatchObject({
      code: expect.stringMatching(/MISSING_PROVENANCE|INVALID_INPUT/),
      suggestion: expect.any(String),
    });
  });

  it('createNote → getNote full', async () => {
    const { version_id } = await tacitus.createNote('notes/fresh', 'hello from the SDK');
    expect(version_id).toBeTruthy();
    const note = await tacitus.getNote({ note_id: 'notes/fresh', format: 'full' });
    expect(note.content).toContain('hello from the SDK');
    expect(note.truncated).toBe(false);
  });

  it('search respects a token budget and returns scored snippets', async () => {
    const hits = await tacitus.search({ query: 'alpha launch', token_budget: 200 });
    expect(hits.length).toBeGreaterThan(0);
    expect(hits[0]!.note_id).toBe('notes/alpha');
    expect(hits[0]!.token_count).toBeGreaterThan(0);
  });

  it('propose → commit → revert, verified on disk', async () => {
    const proposal = await tacitus.proposeChanges([
      { op: 'update', note_id: 'notes/alpha', content: 'changed by the SDK' },
    ]);
    expect(proposal.diff[0]!.after).toContain('changed by the SDK');
    let onDisk = await readFile(join(vault, 'notes', 'alpha.md'), 'utf8');
    expect(onDisk).toContain('Launch checklist'); // dry-run: untouched

    const { version_id } = await tacitus.commitChanges(proposal.change_id);
    onDisk = await readFile(join(vault, 'notes', 'alpha.md'), 'utf8');
    expect(onDisk).toContain('changed by the SDK');

    await tacitus.revert(version_id);
    onDisk = await readFile(join(vault, 'notes', 'alpha.md'), 'utf8');
    expect(onDisk).toContain('Launch checklist');
  });

  it('link is idempotent', async () => {
    await tacitus.link('notes/alpha', 'notes/fresh');
    await tacitus.link('notes/alpha', 'notes/fresh');
    const onDisk = await readFile(join(vault, 'notes', 'alpha.md'), 'utf8');
    expect(onDisk.split('[[notes/fresh]]').length - 1).toBe(1);
  });

  it('auditLog is most-recent-first', async () => {
    const entries = await tacitus.auditLog(50);
    expect(entries.length).toBeGreaterThan(0);
    expect(entries[0]!.action).toBeTruthy();
    const timestamps = entries.map((e) => e.ts);
    const sorted = [...timestamps].sort().reverse();
    expect(timestamps).toEqual(sorted);
  });

  it('native-first tools against the npm server throw TOOL_UNAVAILABLE', async () => {
    const promise = tacitus.listTasks();
    await expect(promise).rejects.toBeInstanceOf(TacitusToolError);
    await expect(promise).rejects.toMatchObject({
      code: 'TOOL_UNAVAILABLE',
      suggestion: expect.stringContaining('tacitus-mcp binary'),
    });
  });
});
