---
title: Use your Obsidian vault as an MCP server (no plugin)
description: Point any MCP agent at your Obsidian vault: wikilink graph, Bases-like property queries, tasks, templates and typed notes, with plain .md files and no plugin.
date: 2026-11-14
kind: guide
---

An Obsidian vault is a folder of Markdown files, and that's all an MCP server needs. This guide connects your vault to Claude Code, Claude Desktop or any other MCP client through Tacitus. The agent can then search notes, follow wikilinks, query frontmatter like a database and manage tasks, without an Obsidian plugin and without Obsidian running. Your files stay plain `.md`. All outputs below come from a real run against the Tacitus npm server (0.2.0), trimmed.

## 1. Connect the vault

```bash
# Claude Code
claude mcp add -s user tacitus -- npx -y @dashiro/tacitus-mcp-server ~/Obsidian/MyVault
```

For Claude Desktop, add this to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "tacitus": {
      "command": "npx",
      "args": ["-y", "@dashiro/tacitus-mcp-server", "/Users/you/Obsidian/MyVault"]
    }
  }
}
```

You can use the native binary instead of `npx`; see the [install steps](/guides/claude-code-memory.html#2-add-the-mcp-server-to-claude-code). Both servers re-read the vault on every call, so a note you edit in Obsidian is visible to the agent on its next tool call, with no restart. (npm 0.2.0 indexed the vault only at startup; use 0.2.1 or newer, which `npx -y` picks up by default.)

## 2. What Tacitus adds to your vault

Tacitus never rewrites your notes on its own. It only keeps its own state in one hidden folder:

| Path | What it holds |
|---|---|
| `.tacitus/memory/` | agent memories, one Markdown file each, with their source |
| `.tacitus/history/` | one snapshot per committed change, which is what makes `revert` possible |
| `.tacitus/audit.log` | one JSON line per commit or revert |
| `.tacitus/templates/` | optional: your note templates |
| `.tacitus/types/` | optional: declared note types |

Obsidian doesn't index dot-folders, the same way it handles its own `.obsidian/`, so none of this shows up in your file explorer, graph or search. If you sync the vault with git, commit `.tacitus/memory/`, `templates/` and `types/`. You can ignore `history/` and `audit.log` if you prefer. The [data format](/docs/data-format.html) page documents every file.

## 3. The wikilink graph, as a tool

The agent doesn't have to grep for `[[`. `graph_query` resolves wikilinks the way Obsidian does: by full path first, then by basename. It takes `relation: links | backlinks | neighbors`, and `depth` for neighbors.

`suggest_links` lists the links a note is missing, each with the reasons it was suggested:

```json
// suggest_links { note_id: "pricing" }
{ "note_id": "pricing",
  "suggestions": [
    { "note_id": "people/ana", "title": "Ana", "score": 0.599,
      "reasons": ["title_mentioned", "semantic"],
      "snippet": "# Pricing Pro is 5 EUR per month. Ana owns pricing. ..." },
    { "note_id": "projects/launch", "title": "Launch", "score": 0.568,
      "reasons": ["title_mentioned", "semantic"] } ] }
```

The possible reasons are `title_mentioned`, `semantic`, `shared_tags` and `backlink`, so the agent (or you) can decide which links are worth adding. Adding one goes through `link { from, to }`, which won't add a duplicate.

Renaming a note in Obsidian updates its links, and `rename_note` does the same for the agent. It retargets every wikilink that resolves to the note and keeps aliases and headings:

```json
// rename_note { from: "people/ana", to: "people/ana-pop" }
{ "version_id": "v_1cbb1ef0e69cd769", "from": "people/ana",
  "to": "people/ana-pop", "links_updated_in": 1 }
```

The rename is a single changeset, so one `revert` undoes the move and the link edits together.

## 4. Frontmatter as a database

If you use Obsidian properties or Bases, `properties_query` covers the same ground for an agent. Filters are AND-ed and use the operators `eq`, `ne`, `contains`, `exists`, `not_exists`, `gt`, `lt`, `gte` and `lte`. ISO dates compare correctly:

```json
// properties_query { filters: [ { key: "status", op: "eq", value: "active" },
//                               { key: "due", op: "lt", value: "2026-12-31" } ],
//                    select: ["status", "due"] }
{ "rows": [ { "note_id": "projects/launch", "title": "Launch",
              "properties": { "due": "2026-12-01", "status": "active" },
              "token_count": 25 } ] }
```

Results default to 50 rows. You can sort with `sort_by` and `descending` and cap the output with `token_budget`.

## 5. Declared types keep properties consistent

Queries are only as reliable as your frontmatter. If you describe a type in `.tacitus/types/project.yaml`:

```yaml
description: A project note
properties:
  status: { type: string, required: true, enum: [active, paused, done] }
  due: { type: date }
```

then the agent can't write a malformed `type: project` note:

```json
{ "ok": false, "error": { "code": "SCHEMA_VIOLATION",
  "reason": "Note \"projects/site\" (type \"project\"): \"status\" must be one of [\"active\", \"paused\", \"done\"], got \"someday\".",
  "suggestion": "Fix the frontmatter to match .tacitus/types/project.yaml (see list_types)." } }
```

Every problem is listed at once, so the agent fixes them in one retry. Notes without a `type`, or with a type you haven't declared, aren't checked. A note that already broke the rules before you declared the type can still be tagged or linked. A write is refused only if it adds a new problem. See [declared types](/docs/mcp-api.html#declared-types).

## 6. Tasks, including due dates

Every checklist line in the vault is a task. `list_tasks` reads due dates written as `due:YYYY-MM-DD` or in the Tasks-plugin style `📅 YYYY-MM-DD`, plus inline `#tags`:

```json
// list_tasks { done: false }
{ "tasks": [
    { "note_id": "projects/launch", "line": 4,
      "text": "write the changelog due:2026-11-20 #release",
      "done": false, "due": "2026-11-20", "tags": ["release"] },
    { "note_id": "daily/2026-11-14", "line": 2,
      "text": "ask Ana whether yearly pricing ships with the beta",
      "done": false, "due": null, "tags": [] } ] }
```

`toggle_task` takes `note_id`, `line` and `expect_text` exactly as `list_tasks` returned them. If you edited that line in Obsidian in the meantime, the agent gets a `CONFLICT` instead of checking off the wrong task.

The second task above came from `capture`, which appends to `daily/YYYY-MM-DD.md`. If your daily notes live elsewhere in Obsidian, that's the folder to point your Daily Notes core plugin at, or you can leave the two separate.

## 7. Templates the agent can fill in

Put templates in `.tacitus/templates/`. Each `{{var}}` placeholder becomes a parameter, and `{{date}}`, `{{time}}` and `{{datetime}}` fill themselves in:

```markdown
---
title: "{{title}}"
date: {{date}}
attendees: {{count}}
---
# {{title}}

## Notes

## Actions
```

`list_templates` reports `{ "name": "meeting", "vars": ["title", "count"] }`. Then `create_from_template { template: "meeting", note_id: "meetings/pricing-sync", vars: { title: "Pricing sync", count: 3 } }` writes `attendees: 3` as a number, not a string, because placeholders are filled in before the YAML is parsed. A missing var returns `MISSING_VARS` with the exact list.

## 8. Optional: encrypted sync between devices

If you already sync the vault with iCloud, Syncthing or git, nothing changes: Tacitus works on whatever folder you give it. The native binary also has its own sync, end-to-end encrypted through a relay that only sees ciphertext:

```bash
tacitus-mcp sync init --vault ~/Obsidian/MyVault   # prints your vault code
tacitus-mcp sync run --live --vault ~/Obsidian/MyVault
```

The vault code is the key: anyone who has it can read the vault, and if you lose it the relay's copy can't be decrypted. Your local files are never at risk. Details are in the [sync docs](/docs/sync.html).

## What's next

- [Give Claude Code long-term memory, with sources](/guides/claude-code-memory.html): `remember`, `recall` and a `CLAUDE.md` snippet.
- [Your agent's memory needs provenance](/guides/agent-memory-provenance.html): the design argument behind all this.
- [MCP API reference](/docs/mcp-api.html): all 27 tools.
