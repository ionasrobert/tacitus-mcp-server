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

# force a relay-log compaction now (live sessions also do this on their own)
tacitus-mcp sync compact --vault ~/vault
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

Each room tracks its **durable frontier** (a mirror of exactly what the log
covers): flushes diff against it, so a checkpoint a peer already pushed is
never re-logged. Every room member is also a **witness** — if the author
crashes before its own ~2s flush, a peer that saw the keystrokes
ephemerally checkpoints them durably (on a 3× deadline, so the author
usually wins and the witness flush no-ops). Room materializations are
audited with `origin: "coedit"` (plain sync applies stay `"sync"`; after a
crash, recovered applies are attributed `"sync"` — the attribution is
cosmetic and deliberately not persisted).

## Compaction (bounded relay log)

The relay can't compact its own log — it never sees plaintext, so it can't
merge CRDT updates. Compaction is **client-driven**: a caught-up client
seals its full state (every doc + the tombstone manifest, so deletes
survive) as one snapshot and offers it with `compact {upto_seq, blob}`.
The relay stores the snapshot, drops every log entry at or below
`upto_seq` (seqs never renumber; the previous log is kept one generation
as `log.prev.jsonl`), and replies `compacted {upto_seq}`. Anyone whose
cursor is below the snapshot gets `snapshot {upto_seq, blob}` before the
tail — a snapshot is just a big idempotent update, so overlap is harmless
and a fresh device converges from snapshot + tail alone.

Live sessions compact automatically: `welcome.log_bytes` reports the log's
size, and a caught-up session whose threshold (default 4 MB) is exceeded
offers a snapshot once per session. `sync compact` forces one.

Trust: the covered seq also travels **inside** the ciphertext
(`snapshot_upto`) and clients advance their cursor only from that inner
value — a hostile relay can't pin a forged `upto_seq` onto some other blob
to leapfrog a client past entries it never delivered. (A hostile relay can
still *withhold* data; that's inherent to relays and unchanged.)

Limits: a snapshot that fits 32 MB travels as the single blob above,
byte-identical to 0.22. Bigger vaults chunk (0.23+): docs are packed into
separately-encrypted **parts** (≤32 MB each, ≤512 MB total — a snapshot
never outgrows the log it replaces). Each part's set membership
(`snapshot_part {upto, idx, of}`) travels inside its ciphertext, and a
client advances its cursor ONLY after applying a complete consistent set —
a relay withholding or splicing parts can stall a client, never skip it
past content. The one remaining ceiling is a single note whose sealed
CRDT state exceeds 32 MB (see Caveats).

## Protocol (for relay implementers)

WebSocket, JSON text frames, blobs base64. Client → `hello {vault_id,
token, since_seq, caps}`; server → `welcome {latest_seq, caps, log_bytes}`
+ backlog `update {seq, blob}`… then live updates. Client `push {blob}` →
`ack {seq}` + fanout to ALL of the vault's connections, pusher included
(cursors advance only through the update stream). Auth is
trust-on-first-use per vault. Per-vault append-only JSONL log, fsynced;
512 MB backstop cap, bounded in practice by compaction.

**Extensions & compatibility:** new message *variants* are parse errors for
old peers, so they are capability-gated: a client lists what it speaks in
`hello.caps`, the relay lists what it supports in `welcome.caps` (both
fields default to empty — 0.19 peers interoperate untouched). `presence
{blob}` (both directions) is the first extension: the relay fans it out
ONLY to connections that advertised the cap, never logs it, never assigns a
seq, never acks it, and drops blobs over 8 KiB. Clients skip well-formed
frames with unknown tags instead of dying, so future extensions stay
deployable.

The second extension is `compact` (see above): `compact {upto_seq, blob}` →
`compacted {upto_seq}` or a structured refusal (`compact_stale`,
`compact_ahead`, `snapshot_too_large`); `snapshot {upto_seq, blob}` is only
ever sent to connections that advertised the cap. A capless client whose
`since_seq` is below the snapshot gets `err {code: "compacted"}` — an
honest fatal error instead of a silent gap (0.21 and older clients below a
compaction must upgrade; at or past it they work untouched). WS frame
limits are raised to 64 MiB on both ends to fit snapshots.

The third extension is `compact2` (0.23+), a strict superset of `compact`
for chunked snapshots: the client uploads `compact_part {upto_seq, idx,
of, blob}` in order 0..of-1 on one connection; the relay stages parts on
disk, commits atomically on the last one, and replies `compacted` or a
structured refusal (`compact_part_order`, `compact_part_too_large`,
`compact_too_large`, plus the stale/ahead pair). A disconnect mid-upload
discards the staging. Multi-part snapshots are served as `snapshot_part
{upto_seq, idx, of, blob}` sequences, and ONLY to compact2 connections —
single-part snapshots keep the legacy `snapshot` frame for every
compact-capable client, so serving below 32 MB is unchanged. A
compact-only client below a multi-part snapshot gets `err {code:
"compacted"}`, exactly like a capless 0.21 client below any snapshot.
Refusals of a compaction offer are advisory on both paths: the log is
untouched, the connection survives, and a live session shrugs them off
(losing the compaction race to another device is that device doing the
job).

## Caveats

- Don't point sync at a vault that's also inside Dropbox/iCloud sync —
  cooperating writers only.
- One sync process per vault per device: don't run `sync run` (live or not)
  against a vault the desktop app is already live-syncing — two engines
  race on the same `.tacitus/sync/` state.
- A single note whose sealed CRDT state exceeds 32 MB can't ride a
  snapshot (a doc never splits across parts) — such a vault doesn't
  compact and grows toward the 512 MB cap. Whole-vault size stopped
  mattering in 0.23: bigger states just chunk.
- Don't downgrade a relay below 0.23 while a chunked snapshot is stored:
  a 0.22 relay can't read the v2 `snapshot.json` and would serve the
  truncated log as if nothing were missing. Single-part snapshots keep
  the 0.22 file format, so vaults under 32 MB are downgrade-safe.
