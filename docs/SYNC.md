# Tacitus Sync (beta)

CRDT sync between your devices — end-to-end encrypted, relay-based,
open-source and self-hostable. The relay never sees plaintext.

## Quick start

```bash
# device 1
tacitus-mcp sync init --vault ~/vault          # prints your vault code
tacitus-mcp sync once --vault ~/vault

# device 2 — paste the code from device 1
tacitus-mcp sync init --vault ~/vault --code tacitus-xxxx-xxxx-…
tacitus-mcp sync once --vault ~/vault

# keep syncing in the background (30s scan interval)
tacitus-mcp sync run --vault ~/vault

# live mode: one persistent relay connection — remote edits land in ~1s
tacitus-mcp sync run --live --vault ~/vault
```

`--live` holds a single WebSocket open instead of a pass every tick: remote
updates apply within a debounce (~250ms) of arriving, local edits are picked
up by the scan tick (`--interval`, default 30s — there is no file watcher by
design). The desktop app uses the same live session and nudges it after
every save, so app-to-app convergence is sub-second.

Default relay: `wss://sync.tacitus.md` (beta, free). Self-host with
`--relay wss://your-host` — the relay is `crates/tacitus-relay`
(Docker image included).

## The vault code IS the key

`sync init` generates a ~160-bit code (`tacitus-xxxx-…`). Everything
derives from it deterministically — same code on any device = same vault:

```
root       = argon2id(code, fixed salt v1)
vault_key  = HKDF(root, "tacitus/v1/vault-key")     # XChaCha20-Poly1305
vault_id   = HKDF(root, "tacitus/v1/vault-id")      # all the relay learns
auth_token = HKDF(root, "tacitus/v1/relay-token")   # one-way from root
```

- Anyone with the code can read (and write) the vault. Share it like a key.
- **Lose the code and the relay's copy is permanently undecryptable.** Your
  local files are plain Markdown on disk and are never at risk — re-run
  `sync init` for a fresh code and vault.
- Note ids, device ids, and contents travel inside the ciphertext. The relay
  sees: vault_id, token, sequence numbers, blob sizes, timestamps, IPs.

## What syncs

- Notes (`**/*.md`, `.tacitus/` excluded)
- Agent memories (`.tacitus/memory/*.md`) — your agents' memory follows you
- NOT synced (device-local by design): history, audit log, embedding
  vectors, templates. Attachments (non-`.md` files) are not synced in v1.

## Merge semantics — "no conflicts, no lost edits", precisely

Each note is a text CRDT (yrs). Local edits are captured as splices at scan
time; concurrent edits always merge with both sides preserved — no conflict
files, no blocked sync. Edits to the same few characters merge
deterministically but can interleave; those notes are flagged for a human
glance. Deletes are causal: an edit the deleter hadn't seen resurrects the
note (edit wins); deleting after seeing the edits sticks.

Remote changes are applied through the same transactional writer agents
use: versioned in `.tacitus/history/`, revertible, and audited with
`origin: "sync"`. Bulk bootstrap (>200 notes) bypasses history and records
one line in `.tacitus/sync/sync.log`.

## Presence (who's online)

Devices in a vault see each other live: online, which note, editing or
not. Presence is **ephemeral** — it never enters the relay log, carries no
sequence number, and gets no ack. Payloads are E2E-encrypted like updates,
under a separate AAD domain (`"{vault_id}#presence"`), so the relay learns
nothing and a sync blob can never be replayed as presence. Departure is a
`gone` goodbye on clean shutdown, or a 45s TTL when a device crashes.
Requires a 0.20+ relay; against an older relay presence is silently off
(sync itself is unaffected — see compatibility below).

## Co-editing (keystroke level)

Two devices with the same note open co-edit live: keystroke updates (yjs
v2) travel as ephemeral frames under their own AAD domain
(`"{vault_id}#coedit"`), and a checkpointed diff lands in the durable log
after ~2s of quiet — offline and third devices converge from the log alone,
and a lost ephemeral frame is repaired by the next durable diff. Frames
over 8 KiB (huge pastes) skip the fast tier and go durable immediately.
Requires a 0.20+ relay; older peers ignore the frames (AEAD, like
presence).

## Protocol (for relay implementers)

WebSocket, JSON text frames, blobs base64. Client → `hello {vault_id,
token, since_seq, caps}`; server → `welcome {latest_seq, caps}` + backlog
`update {seq, blob}`… then live updates. Client `push {blob}` → `ack {seq}`
+ fanout to ALL of the vault's connections, pusher included (cursors
advance only through the update stream). Auth is trust-on-first-use per
vault. Per-vault append-only JSONL log, fsynced; 512 MB cap in beta.

**Extensions & compatibility:** new message *variants* are parse errors for
old peers, so they are capability-gated: a client lists what it speaks in
`hello.caps`, the relay lists what it supports in `welcome.caps` (both
fields default to empty — 0.19 peers interoperate untouched). `presence
{blob}` (both directions) is the first extension: the relay fans it out
ONLY to connections that advertised the cap, never logs it, never assigns a
seq, never acks it, and drops blobs over 8 KiB. Clients skip well-formed
frames with unknown tags instead of dying, so future extensions stay
deployable.

## Caveats

- Don't point sync at a vault that's also inside Dropbox/iCloud sync —
  cooperating writers only.
- One sync process per vault per device: don't run `sync run` (live or not)
  against a vault the desktop app is already live-syncing — two engines
  race on the same `.tacitus/sync/` state.
- Compaction isn't implemented yet; very active vaults grow the relay log.
