import { describe, it, expect } from 'vitest';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { serverVersion } from '../src/server';

describe('server metadata', () => {
  it('reports the published package version, not a hardcoded literal', async () => {
    const pkgPath = fileURLToPath(new URL('../package.json', import.meta.url));
    const pkg = JSON.parse(await readFile(pkgPath, 'utf8')) as { version: string };
    expect(serverVersion()).toBe(pkg.version);
    // The bug this replaces: initialize answered 0.1.0 for two releases.
    expect(serverVersion()).not.toBe('0.0.0');
  });
});
