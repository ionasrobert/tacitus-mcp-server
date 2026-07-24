# Tacitus example plugins

Two complete, runnable plugins — **zero dependencies** (just Python 3 / Node
≥ 18). Each demonstrates a pattern from [docs/PLUGINS.md](../docs/PLUGINS.md).
Both need the native server (`tacitus-mcp` ≥ 0.6) on PATH, or pass
`--server /path/to/tacitus-mcp`.

## `vault_digest.py` — read-only analyzer

Least privilege in practice: runs the server with `TACITUS_SCOPE=read-only`,
so it *cannot* modify the vault. Prints a health digest — open tasks by
urgency (overdue / today / upcoming), note statuses from typed frontmatter,
orphan notes, and recent write activity from the audit log.

```bash
python3 examples/vault_digest.py /path/to/vault
```

## `daily_note.mjs` — capturer / scheduled agent

The write path, done right: installs its template pack on first run, creates
today's `daily/YYYY-MM-DD` from it (typed frontmatter, versioned + audited),
and appends a "Due today" section with links to open tasks due today or
overdue. Idempotent — safe to run from cron; a second run changes nothing.
Demonstrates recovering from a structured `CONFLICT` error.

```bash
node examples/daily_note.mjs /path/to/vault --focus "ship the release"
# cron: 0 7 * * * node /path/to/examples/daily_note.mjs /path/to/vault
```

Every write these plugins make shows up in the desktop app's Activity tab
(and `audit_log`), diffable and revertible — that's the point.
