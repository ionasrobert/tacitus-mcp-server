import { mkdir, readdir, readFile, rename, rm, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { parse as parseYaml, stringify as stringifyYaml } from 'yaml';
import { TacitusError } from '../lib/errors';
import { isPlainId } from '../lib/ids';
import { MemorySchema, type Memory } from './types';

/**
 * Persists memories as Markdown + YAML frontmatter under `.tacitus/memory/`
 * — the same shape as Claude Code's flat-file memory. Writes are atomic
 * (temp + rename) and idempotent (file named by stable id).
 */
export class MemoryStore {
  private readonly memoryDir: string;

  constructor(public readonly vaultDir: string) {
    this.memoryDir = join(vaultDir, '.tacitus', 'memory');
  }

  async save(memory: Memory): Promise<void> {
    await mkdir(this.memoryDir, { recursive: true });
    const { content, ...frontmatter } = memory;
    const fm = stringifyYaml(frontmatter).trimEnd();
    const body = `---\n${fm}\n---\n${content}\n`;

    const target = join(this.memoryDir, `${memory.id}.md`);
    const tmp = join(this.memoryDir, `.${memory.id}.tmp`);
    await writeFile(tmp, body, 'utf8');
    await rename(tmp, target); // same id overwrites → idempotent
  }

  async load(): Promise<Memory[]> {
    let files: string[];
    try {
      files = await readdir(this.memoryDir);
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === 'ENOENT') return [];
      throw err;
    }

    const memories: Memory[] = [];
    for (const file of files.sort()) {
      if (!file.endsWith('.md')) continue;
      try {
        const memory = parseMemoryFile(await readFile(join(this.memoryDir, file), 'utf8'));
        if (memory) memories.push(memory);
      } catch {
        // Corrupt file — skip, never crash the whole load.
        continue;
      }
    }
    return memories;
  }

  async remove(id: string): Promise<boolean> {
    if (!isPlainId(id)) {
      // An id like "../../notes/x" would otherwise delete a file outside the store.
      throw new TacitusError({
        code: 'INVALID_INPUT',
        reason: `Invalid memory_id ${JSON.stringify(id)}.`,
        suggestion: 'Use a memory_id returned by remember or recall.',
      });
    }
    try {
      await rm(join(this.memoryDir, `${id}.md`));
      return true;
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === 'ENOENT') return false;
      throw err;
    }
  }
}

function parseMemoryFile(raw: string): Memory | null {
  const match = /^---\n([\s\S]*?)\n---\n?([\s\S]*)$/.exec(raw);
  if (!match) return null;
  const fm = match[1] ?? '';
  const body = match[2] ?? '';
  const meta = parseYaml(fm) as Record<string, unknown>;
  const candidate = { ...meta, content: body.replace(/\n$/, '') };
  const parsed = MemorySchema.safeParse(candidate);
  return parsed.success ? parsed.data : null;
}
