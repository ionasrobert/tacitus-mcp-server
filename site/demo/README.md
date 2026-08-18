# Demo recordings

Both GIFs were produced by **running the software**, not by typing a plausible
transcript into a terminal theme:

- `tacitus-memory.gif` — the `tacitus-mcp` binary driven over stdio through a
  real MCP session: `remember` → `recall` → `propose_changes` →
  `commit_changes` → `revert`. Every id in the frame (`mem_…`, `chg_…`, `v_…`)
  and the audit line are what the binary returned; the diff is the diff it
  computed.
- `tacitus-sync.gif` — a real `tacitus-relay` on localhost with two vaults
  syncing through it. The blobs on the right are read verbatim out of the
  relay's `log.jsonl` after the run, which is why they decode to nothing.

If a claim in a recording ever stops matching the code, the recording is the
thing that is wrong. Regenerate it rather than editing the frame.
