# Changelog

Notable changes per release. Every `vX.Y.Z` tag builds native installers via
GitHub Releases; the npm server (`@dashiro/tacitus-mcp-server`) versions
independently. Sync protocol details live in [docs/SYNC.md](docs/SYNC.md).

## 0.27.1 — 2026-11-14

- Release build only: the Apple Silicon job now runs on `macos-15`.
  GitHub retired the `macos-14` image that dist picks by default, so the
  v0.27.0 tag never produced a release. Code is identical to 0.27.0.

## 0.27.0 + npm 0.2.0 — 2026-11-14

- **Security: ids can no longer leave the vault.** `note_id` was joined onto
  the vault path unchecked in both engines, so `../x`, `/abs/x` or
  `.tacitus/…` could create, overwrite or delete `.md` files outside the
  vault or over Tacitus' own state; `forget` took `memory_id` as a file name
  the same way, and `revert`/`get_version` did the same with `version_id`.
  Every note path now goes through one validator (`INVALID_NOTE_ID`), memory
  and version ids must be generated-shape (`[A-Za-z0-9_-]+`), and the sync
  engine refuses item keys that would escape (`n:../x`, `m:../x`). With
  collab, a peer holding the vault code could otherwise write outside your
  vault.
- **Fix: JS-based MCP clients saw no native tools.** The JS MCP SDK
  (≥ 1.32) rejects the whole `tools/list` response when a property schema is
  a bare `true`. That is what the native server emitted for `frontmatter` on
  `create_note`, `update_note` and `propose_changes`. Those fields now
  declare an object schema, and a test fails on any boolean property schema.
- **The npm server has every tool now (16 → 27).** `suggest_links`,
  `properties_query`, `list_templates`, `create_from_template`, `list_tasks`,
  `toggle_task`, `rename_note`, `delete_note` and `get_version` are ported
  from the native engine, with the same arguments, outputs and error codes.
  A new parity test starts the npm server over a real MCP client and fails if
  its tool names or descriptions differ from the native table.
- **Declared types** (both engines): `.tacitus/types/<name>.yaml` declares
  the properties of notes with `type: <name>` (`string|number|boolean|date|
  list`, `required`, `enum`). Creates/updates are validated at propose and at
  commit; `SCHEMA_VIOLATION` lists every problem at once. An update is
  refused only for problems it introduces, so notes written before their
  type was declared can still be tagged and linked. Undeclared types,
  untyped notes and sync merges are not validated. New tool `list_types`.
- **`capture`** (both engines): append a timestamped bullet or a `- [ ]`
  task to `daily/YYYY-MM-DD` (created on first capture, from a `daily`
  template when present). Versioned + audited, one revert per capture.
- SDK: `listTypes()`, `capture()`; `TOOL_UNAVAILABLE` now only means "your
  server is older than 0.2.0".

## npm 0.1.3 — 2026-10-31

- `@dashiro/tacitus-mcp-server`: `@modelcontextprotocol/sdk` 1.29 → 1.32.2
  (still speaks protocol `2025-11-25`; the stateless `2026-07-28` revision
  waits for SDK support). Package description fixed (25 native tools, not
  24) and the README now states the hosted relay's plans — npm was the one
  channel where Pro was never mentioned.

## 0.26.0 — 2026-08-14

- **The upgrade path exists in the product now.** `sync status` opens a
  read-only probe of the relay (welcome frame, then hang up — no frame
  reaches the engine, so no cursor moves) and prints `storage: <used> of
  <quota> (N%)`; past 80% it prints the checkout link for that vault. New
  `sync upgrade [--plan monthly|yearly] [--open]` prints or opens it on
  demand, and the `quota_exceeded` message carries the link instead of the
  word "upgrade". None of this appears for self-hosted relays — only for
  `sync.tacitus.md`, or wherever `TACITUS_BILLING_URL` points.
- **`tacitus-billing --reconcile`**: rebuild `entitlements.json` from
  Stripe's live subscription list after a lost volume or a webhook outage.
  Dry by default (`--apply` writes), never touches operator comps, and
  yields to any webhook that landed mid-run.
- Terms, privacy and cancellation/refund pages (`site/*.html`) — what
  Stripe asks for at activation and what EU distance-selling law requires;
  linked from the pricing section and the footer, with self-serve
  cancellation through Stripe's hosted customer portal.
- The npm server reported version `0.1.0` in `initialize` regardless of the
  package version; it now reads its own `package.json`.

## 0.25.0 — 2026-08-05

- **Tacitus Sync Pro plumbing**: new `tacitus-billing` sidecar — Stripe
  Checkout + webhooks that rewrite the relay's `entitlements.json`. No user
  accounts anywhere: Pro is per-vault and billing only ever learns the
  vault id, the same opaque 32-hex string the relay already knows. The
  relay itself is untouched (it hot-reloads the file, shipped in 0.24.0).
- Operator docs in [docs/BILLING.md](docs/BILLING.md); the sidecar refuses
  to boot half-configured and refuses to rewrite an entitlements file it
  cannot parse.

## 0.24.0 — 2026-08-05

- **Per-vault storage quotas** on the relay. The old hardcoded 512 MB cap
  becomes the default quota; `entitlements.json` (hot-reloaded, no restart)
  grants per-vault overrides and `TACITUS_RELAY_QUOTA_DEFAULT` sets the
  default — self-hosted relays behave exactly as before.
- New `quota` protocol extension: `welcome.quota_bytes` + a
  `quota_exceeded` refusal (older clients keep receiving `log_full`).
- **Self-healing clients**: a quota refusal no longer kills a live session —
  the client surfaces `QuotaExceeded`, immediately offers a compaction
  (always allowed over quota; it is the cure), re-sends the refused pushes,
  and only fails for real if the sealed state truly cannot fit. This also
  fixes `log_full` permanently poisoning the outbox against 0.23 relays.

## 0.23.0 — 2026-07-27

- **Chunked snapshots (`compact2`)**: compaction snapshots bigger than
  32 MB travel as separately-encrypted parts, staged on relay disk and
  committed atomically — whole-vault size no longer limits compaction.
  Cursors advance only on a complete consistent set: a hostile relay can
  stall a client, never skip it past content.

## 0.22.0 — 2026-07-27

- **Relay log compaction** (`compact`): a caught-up client seals its full
  state as a snapshot; the relay truncates beneath it. Live sessions
  compact automatically past a size threshold.
- Durable co-edit mirror: checkpoints diff against exactly what the log
  covers, so nothing is ever re-logged; witness flushes preserve a crashed
  author's keystrokes. Room writes audited as `origin: "coedit"`.

## 0.21.0 — 2026-07-27

- **Keystroke co-editing**: two devices on the same note merge live via
  ephemeral E2E frames, with a durable checkpoint after ~2s of quiet —
  offline devices converge from the log alone.

## 0.20.0 — 2026-07-27

- **Presence**: who's online, which note, editing or not — ephemeral,
  end-to-end encrypted under its own AAD domain, never logged.
- Tolerant frame parsing: clients skip unknown future frame tags instead of
  dying.

## 0.19.0 — 2026-07-26

- **Live sync** (`sync run --live`): one persistent relay connection per
  vault; remote edits land in about a second. The wire protocol these notes
  keep referring to ("the 0.19 set") is this release's baseline.

Earlier releases (the MCP server itself, memory with provenance,
transactional write-back, plugins, hybrid search) predate this changelog —
see the git history.
