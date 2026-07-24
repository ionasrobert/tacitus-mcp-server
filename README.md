# Tacitus

**Long-term memory for your AI agents — local-first, with provenance.**

[![npm](https://img.shields.io/npm/v/@dashiro/tacitus-mcp-server)](https://www.npmjs.com/package/@dashiro/tacitus-mcp-server)
[![CI](https://github.com/ionasrobert/tacitus-mcp-server/actions/workflows/ci.yml/badge.svg)](https://github.com/ionasrobert/tacitus-mcp-server/actions/workflows/ci.yml)
[![license](https://img.shields.io/npm/l/@dashiro/tacitus-mcp-server)](./LICENSE)

Tacitus is an [MCP](https://modelcontextprotocol.io) server that turns any folder
of Markdown notes into an **agent-native knowledge base**. It gives AI agents
(Claude Code, Claude Desktop, and any MCP client) three things they actually need:

1. **Memory with provenance** — typed, queryable long-term memory. Every fact
   carries its source and is returned within a token budget; contradictions are
   surfaced, not silently resolved.
2. **Retrieval that fits the context window** — search returns ranked snippets
   (never whole notes) under a token budget; `get_note` discloses progressively
   (outline → frontmatter → full); the wikilink graph is a queryable API.
   Hybrid lexical + semantic search, with an optional neural embedder.
3. **Safe write-back** — propose a changeset, preview the diff, commit
   atomically, and revert by version. Read-only scope forbids mutations; every
   write is audited.

Notes stay as plain `.md` files in your folder. No cloud, no lock-in.

## Quick start

```bash
npx -y @dashiro/tacitus-mcp-server /path/to/your/vault
```

### Claude Code

```bash
claude mcp add tacitus -- npx -y @dashiro/tacitus-mcp-server /path/to/your/vault
```

### Claude Desktop (`claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "tacitus": {
      "command": "npx",
      "args": ["-y", "@dashiro/tacitus-mcp-server", "/path/to/your/vault"]
    }
  }
}
```

### Native binary (no Node)

Prefer a single, zero-dependency binary? The Rust server ships prebuilt for
macOS, Linux, and Windows on every release.

```bash
# macOS / Linux — installs `tacitus-mcp` into your Cargo bin dir
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/ionasrobert/tacitus-mcp-server/releases/latest/download/tacitus-mcp-installer.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://github.com/ionasrobert/tacitus-mcp-server/releases/latest/download/tacitus-mcp-installer.ps1 | iex
```

Or grab a `.tar.xz` / `.zip` for your platform from the
[latest release](https://github.com/ionasrobert/tacitus-mcp-server/releases/latest).
Then point any MCP client at the binary instead of `npx`:

```bash
claude mcp add tacitus -- tacitus-mcp /path/to/your/vault
```

The native binary is the flagship server (24 tools; the npm server has the
core 16 — see the table below). Both share the same on-disk formats, so a
vault works with either. Set `TACITUS_SCOPE=read-only` to run the native
server without write permissions.

## Tools

| Group | Tools |
|---|---|
| **Memory** | `remember`, `recall`, `forget` |
| **Retrieval** | `search`, `get_note`, `graph_query`, `list_notes`, `properties_query`* |
| **Write-back** | `propose_changes`, `commit_changes`, `revert`, `rename_note`*, `delete_note`*, `get_version`* |
| **Convenience** | `create_note`, `update_note`, `link`, `tag`, `audit_log` |
| **Templates** | `list_templates`*, `create_from_template`* |
| **Tasks** | `list_tasks`*, `toggle_task`* |
| **Meta** | `capabilities` |

\* Native-Rust-server first (the npm server will catch up):
`properties_query` — Bases-like structured queries over YAML frontmatter
(filters `eq|ne|contains|exists|not_exists|gt|lt|gte|lte`, sort, select,
token_budget). Templates — Markdown files in `.tacitus/templates/` whose
`{{var}}` placeholders form a schema; substitution happens before YAML
parsing so numeric vars stay typed, `{{date}}`/`{{time}}`/`{{datetime}}`
auto-fill, and creation is versioned + audited like any agent write.
Tasks — every checklist line (`- [ ]`) as a typed entity (done, due from
`due:YYYY-MM-DD` or `📅`, #tags), queryable and toggleable; toggling takes
the task text as a concurrency guard so a stale caller gets a CONFLICT
instead of flipping the wrong task. `rename_note` retargets every wikilink
that resolves to the note (alias/heading kept) in one atomic changeset —
a single revert undoes the whole rename; `delete_note` is versioned too.

Every tool validates input with a schema and returns structured, actionable
errors (`{ code, reason, suggestion }`) rather than stack traces.

## For developers: plugins & integrations

**In Tacitus, a plugin is an MCP client** — the tool contract above is the
public API, with permission scoping, versioning, and audit built in.

- [docs/PLUGINS.md](./docs/PLUGINS.md) — integration guide: connect an agent,
  write a plugin in Python/TypeScript, embed the Rust engine, plugin patterns
- [docs/MCP_API.md](./docs/MCP_API.md) — full reference for all 24 tools
  (params, returns, error codes)
- [docs/DATA_FORMAT.md](./docs/DATA_FORMAT.md) — the on-disk format
  (`.tacitus/` internals, stable ids, note conventions)
- [examples/](./examples/) — two complete zero-dependency plugins (Python
  read-only analyzer, Node daily-note cron agent), tested against the binary

## Semantic search (optional neural embeddings)

`search` defaults to **hybrid** mode (lexical + a deterministic, offline
embedder that catches morphological variants). For synonym/paraphrase matching,
opt into a neural embedder:

```bash
npm i @huggingface/transformers
TACITUS_EMBEDDER=transformers npx @dashiro/tacitus-mcp-server /path/to/vault
```

Vectors are cached under `.tacitus/vectors/`. Falls back to the deterministic
embedder if the optional dependency or model isn't available.

## How it stores things

```
your-vault/
├── notes...             ← your Markdown files (untouched format)
└── .tacitus/
    ├── memory/*.md      ← agent memories (Markdown + YAML frontmatter)
    ├── vectors/*.json   ← cached embeddings
    ├── history/*.json   ← version snapshots (for revert)
    └── audit.log        ← JSONL log of every agent write
```

## Development

Polyglot monorepo. The reference server (shipped on npm) is TypeScript in
`packages/mcp-server`. A **native Rust server** in `crates/` provides a
single-binary, zero-runtime-deps build (`crates/tacitus-core` engine +
`crates/tacitus-mcp` rmcp server) — a superset of the TS server (24 vs 16
tools). Its `stable_id` matches the TS engine byte-for-byte, so memory ids are
identical across both engines.

```bash
# TypeScript server
npm ci
npm test          # vitest
npm run typecheck
npm run lint
npm run build     # tsup → packages/mcp-server/dist
npm run eval      # retrieval quality report

# Rust server (native, single binary)
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo run -p tacitus-mcp -- /path/to/vault   # runs the MCP server on stdio
cargo build --release                         # → target/release/tacitus-mcp
```

Cross-platform release binaries are built and published to GitHub Releases by
[cargo-dist](https://opensource.axo.dev/cargo-dist/) (`dist-workspace.toml` +
`.github/workflows/release.yml`) on every `v*` tag.

## License

MIT — see [LICENSE](./LICENSE).
