---
title: Your agent's memory needs provenance
description: Agent memory that forgets where facts came from turns guesses into truths. Why every memory should carry a source, and why contradictions should be shown.
date: 2026-11-14
kind: article
---

Most agent memory today is a pile of sentences. The agent saw something, summarized it, saved it, and the next session reads it back with the same confidence as everything else in the prompt. A month later nobody can say whether "the API rate limit is 100 requests per minute" came from the vendor's docs, from a user who was guessing, or from the agent misreading a log.

That's the failure this article is about, and it's why Tacitus refuses to store a memory without a source. The argument is general, though. It applies to any memory layer you build or adopt.

## A memory without a source is a rumor

Think about how a careful person handles a remembered fact. "I think the deploy freeze starts Friday" is acceptable. "Ana said in Tuesday's standup that the deploy freeze starts Friday" is much better: you can weigh it, check it, or ask Ana. The second version doesn't contain more facts. It contains the *provenance* of the fact.

Language models don't keep that distinction on their own. Once a sentence is in the context window, its origin is gone unless someone wrote it down. A memory layer is where it gets written down, or lost for good.

So the floor is: every memory carries **where it came from** (a file, a URL, a conversation), **who asserted it** (a human or an agent), and **when**. In Tacitus that is the `source` object on `remember`, and leaving it out is an error, not a warning:

```json
{ "code": "MISSING_PROVENANCE",
  "reason": "A memory must carry its source (provenance).",
  "suggestion": "Provide source: { origin, author: \"human\"|\"agent\", timestamp? }." }
```

Making provenance optional sounds friendlier, but it means most memories will have none, because the easiest path is to skip it. Agents take the easiest path the same way people do.

The `author` field deserves a note. "human" versus "agent" is the most useful single bit you can store. A preference the user stated and a conclusion the agent inferred should not carry equal weight, and with that bit recorded they don't have to.

## Contradictions should be shown, not resolved

Facts change. The deploy day moves, the pricing changes, a library gets replaced. Most memory systems handle this by overwriting: the newest write wins, or a similarity check merges the "duplicate". Both are silent, and both are wrong in the cases that matter.

Newest-wins is wrong whenever the newer fact is less reliable: an agent's inference written after the human's explicit statement. Similarity merging is wrong whenever two facts look alike but disagree, which is exactly what a contradiction is.

The alternative is cheap. Give facts that can change a stable `key`, such as `deploy-day`. When two memories share a key but differ in content, return both, plus an explicit conflict:

```json
"conflicts": [ { "key": "deploy-day",
                 "memory_ids": ["mem_4add74f8ddbe73cc", "mem_8a3ab0034c9aeb17"] } ]
```

Now the agent can do what a careful person would: "The standup said Tuesdays only; `docs/RELEASING.md` says any weekday after CI. Which holds?" Each side comes with its source, so the question can be answered. And because conflicts are computed over everything relevant to the query, not only over what fit in the token budget, a small budget can't hide a contradiction.

This costs one round trip to the user now and then. It saves the much more expensive case where the agent confidently acts on the wrong half of a contradiction it never saw.

## Idempotent ids make "remember" safe to retry

Agents retry. Tool calls time out, sessions restart, the same instruction gets processed twice. If every `remember` creates a new record, memory slowly fills with near-duplicates that crowd out everything else in recall.

Tacitus derives a memory's id from what it says and where it came from: type, key, content, origin and author. The timestamp is not part of it. Remembering the same fact from the same source twice returns the same `memory_id` and writes nothing new. The same fact from a *different* source gets a different id, and that's intended: two independent sources agreeing is information worth keeping.

## Budgets are part of correctness

A memory layer that returns "everything relevant" will eventually return more than the agent can read, and the model then drops something without telling you which part. Recall should take a `token_budget` as a hard ceiling. Each returned item should report its own `token_count`, and items that don't fit should be skipped rather than cut mid-sentence. The agent then knows exactly what it received, and can ask for more on purpose.

The same principle applies to notes. Search returns ranked snippets, never whole files, and the agent opens a note in steps: outline, then frontmatter, then full text.

## Writes should be reviewable and reversible

Provenance covers what the agent *knows*. The other half is what the agent *changes*. If an agent can edit your notes, every edit should be:

- **previewed**: a dry run that returns the before and after diff (`propose_changes`);
- **atomic**: the whole changeset or nothing (`commit_changes`);
- **reversible**: one call restores the prior state (`revert`);
- **audited**: an append-only log of who changed what, and when (`audit_log`).

None of this is new. It's what we expect from git and from databases. It's just rarely applied to the files an agent touches. When it is, letting an agent write to your notes stops being a leap of faith.

## The tradeoffs

Provenance has costs:

- **More tokens per memory.** A source object adds tokens to every recalled item. In practice it's a few dozen, which is small next to the cost of acting on a wrong fact, but it isn't zero.
- **Agents must be told to use keys.** Contradiction detection only works for facts filed under the same `key`. Two contradictory memories with no key, or with different keys, won't be flagged. Choosing keys is a habit you have to put in your agent instructions.
- **"Origin" is a free-text field.** Tacitus records the origin the agent gives; it doesn't verify that `docs/RELEASING.md` says what the agent claims. Provenance makes a claim checkable, not true.
- **Surfacing conflicts means asking the user.** That's the point, but if your agent runs unattended, you need a policy for what happens when nobody is there to answer.

None of these is a reason to drop provenance. They're reasons to be precise about what it gives you: memories you can audit and weigh, instead of an ever-growing pile of confident sentences.

## Try it

Tacitus is an MIT-licensed MCP server that runs on a folder of Markdown files and works with Claude Code, Claude Desktop or any MCP client:

```bash
npx -y @dashiro/tacitus-mcp-server ~/agent-memory
```

- [Give Claude Code long-term memory, with sources](/guides/claude-code-memory.html): setup and a `CLAUDE.md` snippet.
- [Use your Obsidian vault as an MCP server](/guides/obsidian-mcp-server.html): the same model on top of an existing vault.
- [MCP API reference](/docs/mcp-api.html#remember): `remember`, `recall` and every other tool.
- Source: [github.com/ionasrobert/tacitus-mcp-server](https://github.com/ionasrobert/tacitus-mcp-server)
