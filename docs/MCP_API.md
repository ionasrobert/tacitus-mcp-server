# Tacitus MCP API Reference

The MCP tool contract **is** Tacitus's public API — the same surface agents,
plugins, scripts, and the desktop app build on. This page documents every tool
of the native server (v0.7.0, 25 tools). The npm server exposes the core 16
(all except those marked **native-first**).

## Transport & envelope

The server speaks [MCP](https://modelcontextprotocol.io) over **stdio**
(JSON-RPC 2.0). Start it with the vault folder as the only argument:

```bash
tacitus-mcp /path/to/vault              # native binary
npx -y @dashiro/tacitus-mcp-server /path/to/vault   # npm (16 tools)
```

Every tool returns one text content block containing JSON with a uniform
envelope:

```jsonc
{ "ok": true,  "data": { ... } }                     // success
{ "ok": false, "error": {                            // failure (isError: true)
    "code": "NOTE_NOT_FOUND",
    "reason": "No note with id \"x\".",
    "suggestion": "Check the id with list_notes."
} }
```

Errors are **structured and actionable** — `code` is machine-matchable,
`suggestion` says what to do differently. No stack traces.

### Permission scope

Set `TACITUS_SCOPE=read-only` in the server's environment to forbid all
mutations: any write tool fails with `PERMISSION_DENIED`, while reads and
`propose_changes` (a dry-run) still work. Default is `read-write`.
`capabilities` reports the active scope.

### Conventions

- **`note_id`** — the note's vault-relative path without `.md`
  (`projects/launch`). Stable across edits; never line numbers.
- **`token_budget`** — a hard output ceiling. Tools that accept it return
  items until the budget is exhausted; every item carries its own
  `token_count` (~4 chars/token heuristic).
- Bounded by default: list-like tools have limits (`search` top_k,
  `properties_query` limit 50, `list_tasks` limit 100, `audit_log` limit 100,
  `get_version` content 500 tokens). Expansion is explicit.

---

## Memory

### `remember`

Store a typed memory. **Provenance is mandatory** — a fact without a source is
not trustworthy. Idempotent: identical content+source ⇒ identical `memory_id`
(safe to retry).

| Param | Type | Notes |
|---|---|---|
| `content` | string | required, non-empty |
| `type` | string | `user` \| `feedback` \| `project` \| `reference` |
| `tags` | string[] | optional |
| `key` | string | optional — memories sharing a key are conflict-checked |
| `source` | object | **required**: `{ origin, author: "human"\|"agent", timestamp? }` (timestamp auto-stamped if omitted) |
| `ttl` | number | optional, seconds |

→ `{ "memory_id": "mem_<16hex>" }`
Errors: `MISSING_PROVENANCE`, `INVALID_TYPE`, `INVALID_INPUT`.

### `recall`

Relevance-ranked recall within a token budget. Conflicting memories (same
`key`, different content) are **surfaced, never silently resolved**.

| Param | Type | Notes |
|---|---|---|
| `query` | string | required |
| `type` | string | optional filter |
| `token_budget` | number | optional hard ceiling |

→ `{ "items": [{ "memory": {id, type, content, tags, key?, source, ttl?}, "score": n, "token_count": n }], "conflicts": [{ "key": "...", "memory_ids": [...] }] }`

### `forget`

`{ memory_id }` → `{ "removed": true|false }`

---

## Retrieval

### `search`

Ranked snippets — never whole notes. `mode`: `hybrid` (default; lexical
precision + semantic recall for morphological variants), `lexical`, or
`semantic`.

| Param | Type |
|---|---|
| `query` | string |
| `mode` | `hybrid` \| `lexical` \| `semantic` (optional) |
| `token_budget` | number (optional) |
| `top_k` | number (optional) |

→ `{ "hits": [{ "note_id", "title", "score", "snippet", "token_count" }] }`

### `get_note`

Progressive disclosure: fetch only as much as you need.

| Param | Type |
|---|---|
| `note_id` | string |
| `format` | `outline` (default) \| `frontmatter_only` \| `full` |
| `max_tokens` | number (optional ceiling; sets `truncated`) |

→ `{ "note_id", "title", "format", "content", "token_count", "truncated" }`
Errors: `NOTE_NOT_FOUND`.

### `graph_query`

Traverse the wikilink graph instead of grepping.

| Param | Type |
|---|---|
| `from` | string (note_id) |
| `relation` | `links` \| `backlinks` \| `neighbors` |
| `depth` | number (default 1; used by `neighbors`) |

→ `{ "from", "relation", "nodes": [{ "note_id", "title" }] }`
Errors: `NOTE_NOT_FOUND`, `INVALID_INPUT`.

### `suggest_links` *(native-first)*

Auto-linking without an LLM: ranked `[[wikilink]]` candidates the note doesn't
link to yet, scored by title mentions in the note, semantic similarity, shared
tags, and existing backlinks. Each row carries machine-readable `reasons`
(`title_mentioned` | `semantic` | `shared_tags` | `backlink`); for a mention,
`snippet` shows the source context where the link would go.

| Param | Type |
|---|---|
| `note_id` | string |
| `top_k` | number (default 5) |
| `min_score` | number 0..1 (default 0.15) |
| `token_budget` | number (hard ceiling) |

→ `{ "note_id", "suggestions": [{ "note_id", "title", "score", "reasons", "snippet", "token_count" }] }`
Errors: `NOTE_NOT_FOUND`.

### `list_notes`

No params → `{ "notes": [{ "note_id", "title", "path" }] }`

### `properties_query` *(native-first)*

Bases-like structured queries over typed YAML frontmatter — "all notes where
status=active and due < 2026-08-01" is a query, not a search.

| Param | Type | Notes |
|---|---|---|
| `filters` | array | `{ key, op, value? }`, AND-ed. Ops: `eq` `ne` `contains` (array membership / case-insensitive substring) `exists` `not_exists` `gt` `lt` `gte` `lte` (numeric when both numbers, else lexicographic — ISO dates work) |
| `select` | string[] | project only these property keys |
| `sort_by` | string | property key; missing values sort last |
| `descending` | bool | default false |
| `limit` | number | default 50 |
| `token_budget` | number | hard ceiling |

→ `{ "rows": [{ "note_id", "title", "properties": {..}, "token_count" }] }`

---

## Write-back (transactional)

The safe path for multi-note mutations: **propose → inspect diff → commit**.
Every commit produces a `version_id` snapshot under `.tacitus/history/` that
`revert` can undo, and appends to the audit log.

### `propose_changes`

Dry-run a changeset — **nothing is written**.

`{ ops: [{ op: "create"|"update"|"delete", note_id, content?, frontmatter? }] }`
→ `{ "change_id": "chg_<16hex>", "diff": [{ "note_id", "op", "before", "after" }] }`

Idempotent (`change_id` is a hash of the changeset). Validation errors:
`CONFLICT` (create over existing), `NOTE_NOT_FOUND` (update/delete of missing).

### `commit_changes`

`{ change_id }` → `{ "version_id": "v_<16hex>" }`
Applies atomically (all-or-nothing with rollback), re-validating against disk.
Errors: `UNKNOWN_CHANGE` (never proposed, or already committed),
`PERMISSION_DENIED`. Note: pending change_ids live in the server process —
propose and commit within the same session.

### `revert`

`{ version_id }` → `{ "reverted": true, "version_id" }`
Restores every note the version touched to its prior state.
Errors: `UNKNOWN_VERSION`, `PERMISSION_DENIED`.

### `rename_note` *(native-first)*

Rename **and retarget every wikilink** that resolves to the note (full-id,
basename, case-insensitive; `|alias` and `#heading`/`#^block` parts kept) —
one atomic changeset; a single `revert` undoes the whole rename.

`{ from, to }` → `{ "version_id", "from", "to", "links_updated_in": n }`
Errors: `NOTE_NOT_FOUND`, `CONFLICT` (target exists), `INVALID_INPUT`.

### `delete_note` *(native-first)*

`{ note_id }` → `{ "version_id" }` — versioned; `revert` restores it.

### `get_version` *(native-first)*

Inspect what a committed version changed — pairs with `audit_log` and
`revert` to close the observability loop.

| Param | Type | Notes |
|---|---|---|
| `version_id` | string | from `audit_log` / `commit_changes` |
| `include_content` | bool | default false (ops-only summary) |
| `max_tokens` | number | per-content clip, default 500 |

→ `{ "version_id", "change_id", "notes": [{ "note_id", "op": "created"|"updated"|"deleted", "before": null|{content, truncated}, "after": ... }] }`
Errors: `UNKNOWN_VERSION`.

---

## Convenience (auto-commit; still versioned + audited)

| Tool | Params | Returns |
|---|---|---|
| `create_note` | `{ note_id, content, frontmatter? }` | `{ version_id }` — `CONFLICT` if it exists |
| `update_note` | `{ note_id, content?, frontmatter? }` | `{ version_id }` — omitted fields keep current values |
| `link` | `{ from, to }` | `{ version_id }` — appends `[[to]]`, idempotent |
| `tag` | `{ note_id, tag }` | `{ version_id }` — frontmatter tag, deduplicated |
| `audit_log` | `{ limit? }` (default 100) | `{ entries: [{ ts, action: "commit"\|"revert", version_id, change_id?, notes[], scope }] }` most-recent-first |

---

## Templates *(native-first)*

Templates are Markdown files in `.tacitus/templates/*.md`; their `{{var}}`
placeholders **are** the schema. Substitution happens on the raw text before
YAML parsing, so `priority: {{p}}` with `p: 3` becomes a real number.
Builtins `{{date}}` (YYYY-MM-DD), `{{time}}` (HH:MM), `{{datetime}}`
(RFC3339) auto-fill.

### `list_templates`

No params → `{ "templates": [{ "name", "vars": ["title", ...] }] }`

### `create_from_template`

`{ template, note_id, vars?: { name: scalar } }` → `{ "version_id", "note_id" }`
Errors: `TEMPLATE_NOT_FOUND`, `MISSING_VARS` (names exactly what's missing —
fixable in one retry), `CONFLICT`, `INVALID_INPUT` (non-scalar var).

---

## Tasks *(native-first)*

Every checklist line (`- [ ] text`, `* [x] text`, indented allowed) anywhere
in the vault is a typed entity. Metadata parsed from the text: due date
(`due:YYYY-MM-DD` or `📅 YYYY-MM-DD`), inline `#tags`.

### `list_tasks`

| Param | Type |
|---|---|
| `done` | bool (false = open, true = done, omit = all) |
| `due_before` / `due_after` | ISO date strings |
| `tag` | string |
| `note_id` | string |
| `limit` | number (default 100) |
| `token_budget` | number |

→ `{ "tasks": [{ "note_id", "line", "text", "done", "due", "tags", "token_count" }] }`
Sorted by due date (undated last), then note_id, then line.

### `toggle_task`

`{ note_id, line, expect_text }` → `{ "version_id" }`
Pass `line` and `text` **exactly as returned by `list_tasks`** — `expect_text`
is an optimistic-concurrency guard: if the line moved or changed you get
`CONFLICT` (re-list and retry) instead of flipping the wrong task.

---

## Meta

### `capabilities`

No params → `{ "server": "tacitus-memory", "version", "tools": [{ name, description }], "permissions": { "scope": "read-write"|"read-only" } }`

Call this first: it tells you exactly what you can do and under which scope —
never guess.

---

## Error codes (complete list)

| Code | Meaning |
|---|---|
| `MISSING_PROVENANCE` | `remember` without `source` |
| `INVALID_TYPE` | bad memory type |
| `INVALID_INPUT` | malformed argument (bad op, bad author, non-scalar var, …) |
| `NOTE_NOT_FOUND` | unknown `note_id` |
| `CONFLICT` | create-over-existing, rename target exists, stale task guard |
| `UNKNOWN_CHANGE` | `commit_changes` with an unknown/consumed `change_id` |
| `UNKNOWN_VERSION` | `revert`/`get_version` with an unknown `version_id` |
| `PERMISSION_DENIED` | mutation under `TACITUS_SCOPE=read-only` |
| `TEMPLATE_NOT_FOUND` | unknown or path-escaping template name |
| `MISSING_VARS` | template placeholders left unfilled (listed in `reason`) |
| `IO_ERROR` | filesystem failure (path/permissions) |
| `INTERNAL` | unexpected server-side failure |
| `INVALID_MANIFEST` | bad `tacitus-plugin.toml` (unknown tool, write tool under read-only scope, entry escape) |
| `PLUGIN_ABI` | wasm guest broke ABI v1 (missing export, wrong version, non-JSON output) |
| `PLUGIN_TRAP` | wasm guest crashed or exceeded its fuel/memory limits |
