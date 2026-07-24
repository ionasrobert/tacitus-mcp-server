# Building on Tacitus — Plugin & Integration Guide

**In Tacitus, a plugin is an MCP client.** There is no privileged plugin API
behind the app's back: the same 25-tool contract serves AI agents, your
scripts, cron jobs, and the desktop app itself. That's a deliberate inversion
of the Obsidian model — instead of plugins with full filesystem access, every
integration gets:

- a **typed, versioned API** ([MCP_API.md](./MCP_API.md)) with structured errors,
- **permission scoping** (`TACITUS_SCOPE=read-only` for anything that only reads),
- **automatic versioning + audit** for every write (users can inspect and
  revert anything your integration does),
- **bounded output** (`token_budget`) so agent integrations never blow their
  context window.

If you can speak JSON-RPC over stdio, you can extend Tacitus in any language.

## 1. Connect an AI agent (zero code)

```bash
# Claude Code
claude mcp add tacitus -- tacitus-mcp /path/to/vault
# or without the native binary:
claude mcp add tacitus -- npx -y @dashiro/tacitus-mcp-server /path/to/vault
```

```jsonc
// Claude Desktop — claude_desktop_config.json
{
  "mcpServers": {
    "tacitus": {
      "command": "tacitus-mcp",
      "args": ["/path/to/vault"],
      "env": { "TACITUS_SCOPE": "read-only" }   // optional: reads only
    }
  }
}
```

The agent discovers everything itself — tell it to call `capabilities` first.

## 2. Write a plugin in Python (no SDK needed)

The whole protocol is newline-delimited JSON-RPC on stdin/stdout. A complete
working client:

```python
import json, subprocess

class Tacitus:
    def __init__(self, vault, read_only=False):
        import os
        env = dict(os.environ)
        if read_only:
            env["TACITUS_SCOPE"] = "read-only"
        self.p = subprocess.Popen(["tacitus-mcp", vault], text=True, env=env,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self.n = 0
        self._rpc("initialize", protocolVersion="2024-11-05",
                  capabilities={}, clientInfo={"name": "my-plugin", "version": "1.0"})
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def _send(self, msg):
        self.p.stdin.write(json.dumps(msg) + "\n"); self.p.stdin.flush()

    def _rpc(self, method, **params):
        self.n += 1
        self._send({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params})
        for line in self.p.stdout:
            msg = json.loads(line)
            if msg.get("id") == self.n:
                return msg["result"]

    def call(self, tool, **args):
        result = self._rpc("tools/call", name=tool, arguments=args)
        payload = json.loads(result["content"][0]["text"])
        if not payload["ok"]:
            raise RuntimeError(f'{payload["error"]["code"]}: {payload["error"]["reason"]}'
                               f' — {payload["error"]["suggestion"]}')
        return payload["data"]

t = Tacitus("/path/to/vault")
for hit in t.call("search", query="launch deadline", token_budget=300)["hits"]:
    print(hit["note_id"], hit["score"], hit["snippet"][:60])
```

## 3. Write a plugin in TypeScript

```ts
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const client = new Client({ name: "my-plugin", version: "1.0.0" });
await client.connect(new StdioClientTransport({
  command: "tacitus-mcp",
  args: ["/path/to/vault"],
}));

const res = await client.callTool({ name: "list_tasks", arguments: { done: false } });
const { ok, data, error } = JSON.parse((res.content as any)[0].text);
if (!ok) throw new Error(`${error.code}: ${error.reason}`);
console.log(data.tasks);
```

## 4. Embed the engine (Rust)

For deep integrations (UIs, sync services), skip the protocol and use
`tacitus-core` directly — it's exactly what the MCP server and the Tacitus
desktop app are built on:

```toml
[dependencies]
tacitus-core = { git = "https://github.com/ionasrobert/tacitus-mcp-server" }
```

```rust
use tacitus_core::vault::{HashingEmbedder, NoteWriter, PermissionScope,
                          SearchArgs, VaultIndex, search_notes};

let index = VaultIndex::build("/path/to/vault".as_ref())?;
let hits = search_notes(&index, "launch", &SearchArgs::default(), &HashingEmbedder::new());
let mut writer = NoteWriter::new("/path/to/vault", PermissionScope::ReadWrite);
writer.create_note("inbox/from-my-tool", "Hello from my integration", None)?;
```

Key modules: `vault` (index, search, graph, properties, tasks, templates,
rename, transactional `NoteWriter`) and `memory` (remember/recall/store).
All mutation helpers are versioned + audited automatically.

## Complete runnable examples

Two zero-dependency plugins live in [`examples/`](../examples/):
[`vault_digest.py`](../examples/vault_digest.py) (read-only analyzer) and
[`daily_note.mjs`](../examples/daily_note.mjs) (capturer/cron agent —
template pack install, CONFLICT recovery, idempotent re-runs). Start there.

## Plugin patterns

**Read-only analyzer** — run with `TACITUS_SCOPE=read-only`; combine
`properties_query` + `graph_query` + `list_tasks` to compute reports (orphan
notes, overdue tasks, stale projects). Zero risk to the vault by construction.

**Inbox capturer** — a script/webhook that turns external events (mail,
bookmarks, meeting transcripts) into notes via `create_from_template`. Ship a
*template pack* (a folder of `.tacitus/templates/*.md`) with your plugin —
templates are data, the safest kind of plugin.

**Scheduled agent** — a cron job that runs an LLM with Tacitus attached:
"summarize yesterday's notes into `daily/YYYY-MM-DD`" or "find and flag
conflicting memories". Writes are auto-versioned, so a bad run is one
`revert` away — check `audit_log` + `get_version` to review what it did.

**Task front-end** — `list_tasks` + `toggle_task` give you a complete,
conflict-guarded task API; render it as a kanban, a menu-bar widget, a TUI.

**Memory curator** — `recall` surfaces conflicts (`conflicts[]`); a plugin can
present them to the user and `forget` the losers.

## Ground rules (please follow these)

1. **Ask for the least privilege.** Reads only? Run the server with
   `TACITUS_SCOPE=read-only`.
2. **Write through the API, not the filesystem.** MCP writes are validated,
   versioned, audited, and revertible; raw writes are none of those. (Formats
   are documented in [DATA_FORMAT.md](./DATA_FORMAT.md) for readers.)
3. **Respect budgets.** Pass `token_budget` when feeding results to an LLM;
   use `get_note` progressively (`outline` → `full`).
4. **Handle errors by `code`.** They're designed for programmatic recovery —
   `MISSING_VARS` lists what to add; `CONFLICT` on `toggle_task` means
   re-list and retry.
5. **One writer at a time.** Don't run several uncoordinated mutating
   integrations against the same vault simultaneously.

## 5. Sandboxed WASM plugins (experimental)

The capability-scoped plugin host is real: crate `tacitus-plugins` runs a
guest wasm module under Wasmtime with **manifest-declared permissions, no
WASI, no ambient filesystem access**. The guest's only doors are two host
functions — and the important one is the whole thesis in miniature:

> **`tacitus.call` *is* `tools/call`.** A sandboxed plugin calls the same
> tools, with the same argument shapes and the same `{ ok, data | error }`
> envelope, as any MCP agent. Capability scoping = a tool allowlist.

### Manifest — `tacitus-plugin.toml`

A plugin is a directory under `<vault>/.tacitus/plugins/<name>/` (directory
name must equal the manifest `name`):

```toml
name = "hello-tacitus"            # ^[a-z0-9][a-z0-9-]*$
version = "0.1.0"
description = "Read-only vault digest."
entry = "hello_tacitus.wasm"      # relative to this file; ".." rejected

[permissions]
scope = "read-only"               # "read-only" | "read-write"
tools = ["search"]                # exact allowlist; anything else → PERMISSION_DENIED
```

Validation happens at load, before any wasm compiles: unknown tools, write
tools under `scope = "read-only"`, and entry paths that escape the plugin
directory are all `INVALID_MANIFEST` with an actionable suggestion.

### ABI v1

Payloads are UTF-8 JSON in guest linear memory, addressed as `(ptr << 32) |
len` packed into an `i64`.

| Guest exports | Signature | Contract |
|---|---|---|
| `memory` | memory | the linear memory |
| `tacitus_abi_version` | `() -> i32` | must return `1` |
| `tacitus_alloc` | `(len: i32) -> i32` | pointer to `len` writable bytes |
| `tacitus_dealloc` | `(ptr: i32, len: i32)` | free (a no-op is fine) |
| `tacitus_run` | `(ptr: i32, len: i32) -> i64` | entry point; input `{"input": …, "plugin": {name, version}}`; returns packed ptr/len of a `{ok, data\|error}` envelope |

| Host imports (module `"tacitus"`) | Signature | Contract |
|---|---|---|
| `call` | `(tool_ptr, tool_len, args_ptr, args_len: i32) -> i64` | run an allowlisted tool; **always** returns an envelope — denial is `PERMISSION_DENIED` data, never a trap |
| `log` | `(level: i32, ptr: i32, len: i32)` | 0=debug 1=info 2=warn 3=error; captured by the host |

**All 25 tools of [MCP_API.md](./MCP_API.md) are reachable in the sandbox** —
the host and the MCP server share one registry (`tacitus_core::tools`), so the
surfaces cannot diverge. The manifest allowlist gates which of them a plugin
may call; the 12 write tools (`remember`, `forget`, `commit_changes`,
`revert`, `create_note`, `update_note`, `link`, `tag`, `create_from_template`,
`toggle_task`, `rename_note`, `delete_note`) additionally require
`scope = "read-write"`. (`propose_changes` is a dry-run and stays available
read-only — committing is the guarded step.) Writes go through the same
transactional `NoteWriter` as every agent — versioned, audited, revertible.

### Limits (host policy — a manifest can never raise them)

Each `run()` gets a deterministic **fuel** budget (default 10⁹ instructions)
and the guest's linear memory is capped (default 64 MiB). A runaway or
crashing guest surfaces as a structured `PLUGIN_TRAP`; a protocol violation
(missing export, wrong ABI version, non-JSON output) as `PLUGIN_ABI`.

### Try it

The reference guest lives at
[`examples/plugins/hello-tacitus/`](../examples/plugins/hello-tacitus/) —
ABI v1 by hand in ~100 lines of Rust, no SDK:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown \
  --manifest-path examples/plugins/hello-tacitus/Cargo.toml
cargo run -p tacitus-plugins --example run_plugin -- \
  /path/to/vault examples/plugins/hello-tacitus '{"query":"launch"}'
```

Roadmap from here: a `tacitus-mcp plugin run|list` CLI, desktop integration
with permission-consent UI, lifecycle hooks (`on_note_saved`), and a TS/Deno
host. If you build something, open an issue — real integrations shape this
design.
