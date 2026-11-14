---
title: Give Claude Code long-term memory, with sources
description: Add persistent, sourced memory to Claude Code with one MCP server: install, a CLAUDE.md snippet, recall under a token budget, and revertible note edits.
date: 2026-11-14
kind: guide
---

Claude Code starts every session without memory of earlier ones, apart from what you put in `CLAUDE.md`. This guide adds a memory layer that lasts across sessions and projects: facts Claude stores with their source, which it can recall under a token budget, kept as plain Markdown in a folder you choose. Setup takes about two minutes. Everything below was run against the Tacitus npm server, version 0.2.0. The JSON is real output, trimmed.

## 1. Pick a folder

Tacitus works on an ordinary folder of Markdown files. You can use an existing notes folder or an Obsidian vault, or start an empty one:

```bash
mkdir -p ~/agent-memory
```

Your notes stay as plain `.md` files. Tacitus keeps its own state, such as memories, history and the audit log, in a hidden `.tacitus/` folder inside the vault. There is no account and no cloud dependency.

## 2. Add the MCP server to Claude Code

With Node 22 or newer:

```bash
claude mcp add -s user tacitus -- npx -y @dashiro/tacitus-mcp-server ~/agent-memory
```

`-s user` registers the server for every project, not only the current directory. A memory that disappears when you `cd` elsewhere isn't long-term.

If you'd rather not depend on Node, install the native binary, a single file for macOS, Linux and Windows:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/ionasrobert/tacitus-mcp-server/releases/latest/download/tacitus-mcp-installer.sh | sh
claude mcp add -s user tacitus -- tacitus-mcp ~/agent-memory
```

Both servers expose the same 27 tools with the same arguments. The native binary also has end-to-end encrypted sync and a read-only mode, covered at the end.

To check the setup, ask Claude to call the `capabilities` tool. It lists every tool and the current permission scope.

## 3. Tell Claude when to remember and recall

An MCP server only gives Claude tools. When to use them is up to you, and `CLAUDE.md` is where you say it. Here is a starting point to paste into `~/.claude/CLAUDE.md`. It's advice to adapt, not a requirement:

```markdown
## Long-term memory (Tacitus MCP)

- At the start of a task, call `recall` with a short query about the task
  and a `token_budget` of about 800. Treat results as claims with sources,
  not as ground truth.
- When I state a durable preference, decision or fact, call `remember` with
  `type` (user | feedback | project | reference), a stable `key` when the
  fact can change later (e.g. "deploy-day"), and
  `source: { origin, author }`: origin says where it came from (a file path,
  a URL, "conversation 2026-11-14"), author is "human" or "agent".
- If `recall` returns `conflicts`, show me both memories and ask which one
  holds. Never pick one silently.
- For quick notes and follow-ups, use `capture` (task: true for to-dos).
```

The `key` matters. Two memories with the same `key` and different content are reported as a conflict. Without a key, Tacitus can't tell that two facts contradict each other.

## 4. What a remember and recall round trip looks like

`remember` won't store a fact without its source:

```json
{ "ok": false, "error": {
  "code": "MISSING_PROVENANCE",
  "reason": "A memory must carry its source (provenance).",
  "suggestion": "Provide source: { origin, author: \"human\"|\"agent\", timestamp? }." } }
```

With a source, you get back a stable id:

```json
// remember { content: "Deploys go out on Tuesdays only.", type: "project",
//            key: "deploy-day", source: { origin: "standup 2026-11-14", author: "human" } }
{ "ok": true, "data": { "memory_id": "mem_8a3ab0034c9aeb17" } }
```

Calling the same thing again returns the same `memory_id` and creates no duplicate. The id is derived from type, key, content, origin and author, so Claude can retry safely.

Now suppose a later session learns something different from a document, and you ask Claude when deploys go out:

```json
// recall { query: "when do deploys go out", token_budget: 200 }
{ "items": [
    { "memory": { "content": "Deploys can go out any weekday after CI is green.",
                  "key": "deploy-day",
                  "source": { "origin": "docs/RELEASING.md", "author": "agent",
                              "timestamp": "2026-11-14T12:43:39.333Z" } },
      "score": 3, "token_count": 13 },
    { "memory": { "content": "Deploys go out on Tuesdays only.",
                  "key": "deploy-day",
                  "source": { "origin": "standup 2026-11-14", "author": "human",
                              "timestamp": "2026-11-14T12:43:39.331Z" } },
      "score": 3, "token_count": 8 } ],
  "conflicts": [ { "key": "deploy-day",
                   "memory_ids": ["mem_4add74f8ddbe73cc", "mem_8a3ab0034c9aeb17"] } ] }
```

Note three things:

- **Every item carries its source.** Claude can say "the standup said Tuesdays, but RELEASING.md says any weekday", which beats stating either one as fact.
- **The contradiction is in `conflicts`.** Neither memory was overwritten and Tacitus didn't pick a winner. Conflicts are computed over everything relevant to the query, so a tight `token_budget` can't hide one.
- **`token_budget` is a hard ceiling.** Items that don't fit are skipped, and each one reports its own `token_count`.

Memories are stored as Markdown with YAML frontmatter in `.tacitus/memory/<id>.md`. You can read them, grep them and commit them to git. To drop one, Claude calls `forget` with its `memory_id`.

## 5. Let Claude edit notes safely

Recall covers what Claude knows. Your notes also need protecting when Claude changes them. Tacitus writes in two steps:

```json
// propose_changes { ops: [{ op: "update", note_id: "pricing",
//                           content: "Pro is 5 EUR per month or 48 EUR per year." }] }
{ "change_id": "chg_e218429c49b297d6",
  "diff": [ { "note_id": "pricing", "op": "update",
              "before": "# Pricing\n\nPro is 5 EUR per month. ...",
              "after": "Pro is 5 EUR per month or 48 EUR per year.\n" } ] }
```

Nothing is written until `commit_changes { change_id }`, which applies the whole changeset or none of it and returns a `version_id`. `revert { version_id }` restores the previous state. `audit_log` shows what the agent did:

```json
{ "entries": [ { "ts": "2026-11-14T12:43:39.342Z", "action": "commit",
                 "version_id": "v_ab0d4252ad7525eb", "change_id": "chg_e218429c49b297d6",
                 "notes": ["pricing"], "scope": "read-write" } ] }
```

A useful rule for `CLAUDE.md`: "Show me the `propose_changes` diff before you commit anything outside `daily/`."

## 6. Quick capture

You don't always want Claude to decide where a note belongs. `capture` appends one line to today's daily note:

```json
// capture { text: "ask Ana whether yearly pricing ships with the beta", task: true }
{ "note_id": "daily/2026-11-14", "version_id": "v_361bdde1ac736522",
  "line": "- [ ] ask Ana whether yearly pricing ships with the beta", "created": true }
```

With `task: true` the line is a checklist item, so `list_tasks` finds it later, including a `due:2026-11-20` if you put one in the text. Without it, the line is a timestamped bullet. Each capture is its own version, so one `revert` undoes it.

## 7. Optional: read-only and multi-device

- **Read-only mode (native binary).** Start the server with `TACITUS_SCOPE=read-only`, for example `claude mcp add -s user tacitus -e TACITUS_SCOPE=read-only -- tacitus-mcp ~/agent-memory`. Writes (`commit_changes`, `create_note`, `capture` and the others, and from 0.27.2 also `remember` and `forget`) then fail with `PERMISSION_DENIED`, and `propose_changes` still works as a dry run. The npm server always runs read-write.
- **Same memory on every machine (native binary).** `tacitus-mcp sync init --vault ~/agent-memory` prints a vault code. Run `sync init --code …` on the second machine. The relay only stores ciphertext; see [how sync works](/docs/sync.html).

## What's next

- [Use your Obsidian vault as an MCP server](/guides/obsidian-mcp-server.html): wikilinks, properties, tasks and templates, as tools.
- [Your agent's memory needs provenance](/guides/agent-memory-provenance.html): why every memory carries its source and why contradictions are reported instead of resolved.
- [MCP API reference](/docs/mcp-api.html): every tool, argument and error code.
