# Tacitus On-Disk Format

Everything Tacitus knows lives in **plain files inside the user's vault** — no
database, no cloud, no lock-in. This spec lets you build tools that read (or
carefully write) the same data. When in doubt, prefer the MCP tools: they add
validation, versioning, and audit that raw file writes bypass.

```
your-vault/
├── **/*.md                  ← notes (user-owned, standard Markdown + YAML)
└── .tacitus/                ← Tacitus's own data (never indexed as notes)
    ├── memory/*.md          ← agent memories
    ├── history/*.json       ← version snapshots (revert)
    ├── audit.log            ← JSONL log of every write
    ├── templates/*.md       ← note templates
    └── vectors/*.json       ← embedding cache (npm neural embedder)
```

## Notes

- A note is any `*.md` file in the vault (recursively), except under
  `.tacitus/`.
- **`note_id`** = vault-relative path without the `.md` extension, `/`
  separators (`projects/launch`). This is the stable address used everywhere.
- **Frontmatter**: optional leading `---\n<YAML>\n---\n` block. Typed values
  (numbers, booleans, arrays) are preserved and queryable via
  `properties_query`.
- **Title resolution**: frontmatter `title` → first `# H1` → filename.
- **Wikilinks**: `[[target]]`, `[[target|alias]]`, `[[target#heading]]`,
  `[[target#^block-id]]`. Target resolution: exact note_id first, then
  basename match (case-insensitive) — `[[Launch]]` finds `projects/launch`.
- **Tags**: frontmatter `tags:` (array or delimited string) plus inline
  `#tag` (`[A-Za-z0-9_/-]+`).
- **Tasks**: any checklist line `- [ ] text` / `* [x] text` (leading
  whitespace allowed). Parsed metadata inside the text: due date as
  `due:YYYY-MM-DD` or `📅 YYYY-MM-DD`; inline `#tags`.

## Stable ids

`stable_id(seed, prefix)` = `prefix + "_" + first 16 hex chars of
sha256(seed)`. Deterministic and **bit-identical between the Rust and
TypeScript engines**, so ids are portable across both.

| Prefix | Entity | Seed |
|---|---|---|
| `mem_` | memory | `"{type} {key} {content} {origin} {author}"` (timestamp excluded → idempotent) |
| `chg_` | proposed changeset | JSON of the changeset |
| `v_` | committed version | `"{change_id}:{unix_millis}"` |

## `.tacitus/memory/<memory_id>.md`

One memory per file, Markdown + YAML frontmatter; the body is the memory
content. Files are written atomically (temp + rename); corrupt files are
skipped on load, never fatal.

```markdown
---
id: mem_2c26b46b68ffc68f
type: project            # user | feedback | project | reference
tags: [launch]
key: launch-date         # optional; same-key disagreements are surfaced as conflicts
source:                  # provenance is MANDATORY
  origin: session-2026-07-24
  author: agent          # human | agent
  timestamp: 2026-07-24T10:00:00Z
ttl: 2592000             # optional, seconds
---
The launch is planned for March.
```

## `.tacitus/history/<version_id>.json`

One snapshot per committed version — enough to undo it. `before`/`after` map
`note_id` → the note's full raw file contents, or `null` for
absent/deleted.

```json
{
  "version_id": "v_8bb16e08588a2576",
  "change_id": "chg_3327cebb0ed078e6",
  "before": { "projects/launch": null },
  "after":  { "projects/launch": "---\ntitle: Launch\n---\nBody.\n" }
}
```

Reverting applies `before`; a revert does not create a new snapshot.

## `.tacitus/audit.log`

Append-only JSONL — one line per commit or revert. This is the trust surface:
every agent (and app) write lands here.

```json
{"ts":"2026-07-24T10:00:00Z","action":"commit","version_id":"v_8bb1...","change_id":"chg_3327...","notes":["projects/launch"],"scope":"read-write"}
{"ts":"2026-07-24T10:05:00Z","action":"revert","version_id":"v_8bb1...","notes":["projects/launch"],"scope":"read-write"}
```

## `.tacitus/templates/<name>.md`

A template is an ordinary Markdown file whose `{{var}}` placeholders form its
schema (vars are inferred by scanning — no separate manifest). Rendering
substitutes on the **raw text, frontmatter included**, so numeric placeholders
produce typed YAML. Builtins `{{date}}`, `{{time}}`, `{{datetime}}` auto-fill
when not supplied.

```markdown
---
title: "{{title}}"
status: draft
priority: {{p}}
---
# {{title}}
Created {{date}}.
```

## `.tacitus/vectors/*.json`

Embedding cache used by the npm server's opt-in neural embedder
(`TACITUS_EMBEDDER=transformers`): a JSON object mapping content-hash keys
(`v_<16hex>` of the text) to vectors. Safe to delete — it regenerates.

## Writing directly vs through MCP

Direct file writes are legitimate (it's the user's vault) but bypass
versioning and audit. Rules of thumb:

- **Reads**: parse freely; the formats above are stable.
- **Note writes from tools/plugins**: go through MCP (`create_note`,
  `propose_changes`, …) so users get diffs, versions, and an audit trail.
- If you must write files directly, write atomically (temp file + rename) —
  that's what both engines and the desktop app do.
- Concurrency: Tacitus assumes cooperating writers on a local filesystem.
  `commit_changes` re-validates against disk before applying, and
  `toggle_task` carries a text guard, but there is no cross-process lock —
  avoid running multiple uncoordinated writers against one vault.
