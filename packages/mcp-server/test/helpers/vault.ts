import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

/**
 * Writes a tiny sample vault and returns its dir. Link graph:
 *   index      → projects/launch, ideas
 *   ideas      → projects/launch
 *   projects/launch → ideas
 */
export async function makeVault(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), 'mv-vault-'));

  await writeFile(
    join(dir, 'index.md'),
    '---\ntitle: Home\n---\n# Home\n\nWelcome. The launch is coming. See [[projects/launch]] and [[ideas]].\n',
    'utf8',
  );

  await mkdir(join(dir, 'projects'), { recursive: true });
  await writeFile(
    join(dir, 'projects', 'launch.md'),
    '---\ntitle: Launch\ntags: [project]\n---\n\nLaunch overview. Related: [[ideas]].\n\n## Timeline\nDates.\n\n## Risks\nMitigations.\n',
    'utf8',
  );

  await writeFile(
    join(dir, 'ideas.md'),
    '---\ntitle: Ideas\ntags: [idea]\n---\n# Ideas\n\nThe launch deadline is in March. See [[projects/launch]].\n',
    'utf8',
  );

  return dir;
}
