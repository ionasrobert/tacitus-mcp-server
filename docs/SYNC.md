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
```

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

## Protocol (for relay implementers)

WebSocket, JSON text frames, blobs base64. Client → `hello {vault_id,
token, since_seq}`; server → `welcome {latest_seq}` + backlog `update {seq,
blob}`… then live updates. Client `push {blob}` → `ack {seq}` + fanout to
ALL of the vault's connections, pusher included (cursors advance only
through the update stream). Auth is trust-on-first-use per vault. Per-vault
append-only JSONL log, fsynced; 512 MB cap in beta.

## Caveats

- Don't point sync at a vault that's also inside Dropbox/iCloud sync —
  cooperating writers only.
- One `sync run` process per vault per device.
- Compaction isn't implemented yet; very active vaults grow the relay log.
