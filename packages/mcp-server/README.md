# @dashiro/tacitus-mcp-server

**Long-term memory for your AI agents — local-first, with provenance.**

Site: **[tacitus.md](https://tacitus.md)** · MIT · no telemetry, no account.

> **All 27 tools, same contract as the native binary.** The **native binary**
> adds [end-to-end encrypted multi-device sync](#multi-device-sync-native-binary-beta)
> and needs zero runtime deps; ships for macOS/Linux/Windows via
> [GitHub Releases](https://github.com/ionasrobert/tacitus-mcp-server/releases/latest):
>
> ```bash
> curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ionasrobert/tacitus-mcp-server/releases/latest/download/tacitus-mcp-installer.sh | sh
> ```
>
> Both share the same on-disk format — a vault works with either.
> Docs: [MCP API](https://github.com/ionasrobert/tacitus-mcp-server/tree/main/docs)

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
| **Retrieval** | `search`, `get_note`, `graph_query`, `suggest_links`, `list_notes`, `properties_query` |
| **Write-back** | `propose_changes`, `commit_changes`, `revert`, `rename_note`, `delete_note`, `get_version` |
| **Convenience** | `create_note`, `update_note`, `link`, `tag`, `audit_log` |
| **Types** | `list_types` |
| **Capture** | `capture` |
| **Templates** | `list_templates`, `create_from_template` |
| **Tasks** | `list_tasks`, `toggle_task` |
| **Meta** | `capabilities` |

Declared types: put `.tacitus/types/article.yaml` (`properties: { status:
{ type: string, required: true, enum: [idea, draft] } }`) in the vault and any
write to a note with `type: article` is validated — a bad write gets a
`SCHEMA_VIOLATION` listing every problem. `capture` appends a timestamped
bullet or a `- [ ]` task to `daily/YYYY-MM-DD`.

Every tool validates input with a schema and returns structured, actionable
errors (`{ code, reason, suggestion }`) rather than stack traces.

## Multi-device sync (native binary, beta)

The native binary adds CRDT sync through a relay that stores **only
ciphertext** — the vault code is the key, everything derives from it locally,
and the relay learns exactly one thing: a 32-character vault id.

```bash
tacitus-mcp sync init --vault ~/vault     # prints the vault code
tacitus-mcp sync run --live --vault ~/vault
```

Two devices with the same note open co-edit at keystroke level, still
end-to-end encrypted. The relay is one MIT Rust crate with a Docker image —
self-host it with no limits, or use the hosted one:

| Hosted relay | Quota per vault | Price |
|---|---|---|
| Free | 100 MB | €0 |
| **Pro** | 1 GB | €5/month or €48/year — [tacitus.md/#pro](https://tacitus.md/#pro) |

No account: Pro is bought per vault id, and `tacitus-mcp sync status` shows
how much of the quota a vault uses. Protocol and threat model:
[docs/SYNC.md](https://github.com/ionasrobert/tacitus-mcp-server/blob/main/docs/SYNC.md).

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
