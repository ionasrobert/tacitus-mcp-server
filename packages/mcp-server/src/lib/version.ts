import { createRequire } from 'node:module';

/**
 * The published package version, reported in `initialize` and by the
 * `capabilities` tool. Read from package.json rather than hardcoded — a stale
 * literal told every client we were 0.1.0 for several releases. The bundle is
 * one file (dist/index.js → ../package.json); from source this module sits in
 * src/lib/ (→ ../../package.json), so both are tried.
 */
export function serverVersion(): string {
  const require = createRequire(import.meta.url);
  for (const rel of ['../package.json', '../../package.json']) {
    try {
      const pkg = require(rel) as { name?: string; version?: string };
      if (pkg.name === '@dashiro/tacitus-mcp-server' && pkg.version) return pkg.version;
    } catch {
      // try the next location
    }
  }
  // Never let metadata kill a server that otherwise works.
  return '0.0.0';
}
