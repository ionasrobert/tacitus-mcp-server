# @dashiro/tacitus-mcp-server

**Long-term memory for your AI agents — local-first, with provenance.**

An [MCP](https://modelcontextprotocol.io) server that turns any folder of
Markdown notes into an agent-native knowledge base. It gives AI agents
(Claude Code, Claude Desktop, and any MCP client) three things they actually
need:

1. **Memory with provenance** — typed, queryable long-term memory. Every fact
   carries its source and is returned within a token budget; contradictions are
   surfaced, not silently resolved.
2. **Retrieval that fits the context window** — search returns ranked snippets
   (never whole notes) under a token budget; `get_note` discloses progressively
   (outline → frontmatter → full); the wikilink graph is a queryable API.
3. **Safe write-back** — propose a changeset, preview the diff, commit
   atomically, and revert by version. Read-only scope forbids mutations; every
   write is audited.

Notes stay as plain `.md` files in your folder. No cloud, no lock-in.

## Quick start

```bash
npx @dashiro/tacitus-mcp-server /path/to/your/vault
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

## Semantic search (optional neural embeddings)

`search` defaults to **hybrid** mode (lexical + a deterministic, offline
embedder that catches morphological variants). For true synonym/paraphrase
matching, opt into a neural embedder:

```bash
npm i @huggingface/transformers
TACITUS_EMBEDDER=transformers npx @dashiro/tacitus-mcp-server /path/to/vault
```

Vectors are cached under `.tacitus/vectors/`. If the optional dependency or
model isn't available, it falls back to the deterministic embedder automatically.

## Tools

| Group | Tools |
|---|---|
| **Memory** | `remember`, `recall`, `forget` |
| **Retrieval** | `search`, `get_note`, `graph_query`, `list_notes` |
| **Write-back** | `propose_changes`, `commit_changes`, `revert` |
| **Convenience** | `create_note`, `update_note`, `link`, `tag`, `audit_log` |
| **Meta** | `capabilities` |

Every tool validates input with a schema and returns structured, actionable
errors (`{ code, reason, suggestion }`) rather than stack traces.

## How it stores things

```
your-vault/
├── notes...             ← your Markdown files (untouched format)
└── .tacitus/
    ├── memory/*.md      ← agent memories (Markdown + YAML frontmatter)
    ├── history/*.json   ← version snapshots (for revert)
    └── audit.log        ← JSONL log of every agent write
```

## License

MIT
