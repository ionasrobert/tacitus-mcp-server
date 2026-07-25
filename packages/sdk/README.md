# @dashiro/tacitus-sdk

Typed TypeScript client for the [Tacitus](https://tacitus.md) MCP server —
every tool as a typed method, structured errors you can branch on, one import.

```ts
import { TacitusClient, TacitusToolError } from '@dashiro/tacitus-sdk';

// Native binary (all 25 tools) — https://github.com/ionasrobert/tacitus-mcp-server/releases
const tacitus = await TacitusClient.spawn({ vault: '/path/to/vault' });

// …or the npm server (core 16 tools), zero install:
// await TacitusClient.spawn({
//   vault: '/path/to/vault',
//   command: 'npx',
//   args: ['-y', '@dashiro/tacitus-mcp-server'],
// });

const hits = await tacitus.search({ query: 'client X', token_budget: 500 });
const note = await tacitus.getNote({ note_id: hits[0].note_id, format: 'full' });

await tacitus.remember({
  content: 'Client X prefers async communication.',
  type: 'user',
  source: { origin: 'crm-import', author: 'agent' },
});

try {
  await tacitus.commitChanges('stale-change-id');
} catch (err) {
  if (err instanceof TacitusToolError && err.code === 'UNKNOWN_CHANGE') {
    // every error carries {code, reason, suggestion} — recover programmatically
  }
}

await tacitus.close();
```

## What you get

- **25 typed methods** mirroring [docs/MCP_API.md](https://github.com/ionasrobert/tacitus-mcp-server/blob/main/docs/MCP_API.md):
  retrieval (`search`, `getNote`, `graphQuery`, `suggestLinks`, `listNotes`),
  agent memory with provenance (`remember`, `recall`, `forget`),
  transactional write-back (`proposeChanges` → `commitChanges` → `revert`,
  plus `createNote`/`updateNote`/`link`/`tag`/`auditLog`), properties, tasks,
  templates, rename/delete, `getVersion`, `capabilities`.
- **Envelope unwrapping**: tools return `{ ok, data | error }` — the SDK gives
  you `data` typed, or throws `TacitusToolError { code, reason, suggestion }`.
- **Any transport**: `TacitusClient.connect(transport)` accepts any MCP
  transport if you don't want the built-in stdio `spawn()`.
- `call<T>(tool, args)` as a raw escape hatch.

## Server surfaces

| Server | Tools | Scope control |
|---|---|---|
| `tacitus-mcp` native binary (GitHub Releases) | all 25 | `TACITUS_SCOPE=read-only` honored |
| `@dashiro/tacitus-mcp-server` (npx) | core 16 | always read-write |

Calling a native-first tool against the npm server throws
`TacitusToolError` with code `TOOL_UNAVAILABLE`.

MIT · part of the [tacitus-mcp-server](https://github.com/ionasrobert/tacitus-mcp-server) monorepo.
