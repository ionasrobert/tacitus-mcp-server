#!/usr/bin/env python3
"""Vault digest — a complete, runnable Tacitus plugin (read-only analyzer).

Connects to the Tacitus MCP server with TACITUS_SCOPE=read-only (least
privilege: this plugin CANNOT modify the vault, by construction) and prints a
health digest: tasks by urgency, note statuses, orphan notes, recent write
activity.

Usage:
    python3 examples/vault_digest.py /path/to/vault
    python3 examples/vault_digest.py /path/to/vault --server ./target/release/tacitus-mcp

Requires the native server (tacitus-mcp >= 0.6) on PATH, or pass --server.
No Python dependencies — the MCP stdio protocol is just JSON lines.
"""

import argparse
import datetime
import json
import os
import subprocess
import sys
from collections import Counter


class Tacitus:
    """Minimal MCP stdio client (newline-delimited JSON-RPC 2.0)."""

    def __init__(self, server_cmd, vault, read_only=False):
        env = dict(os.environ)
        if read_only:
            env["TACITUS_SCOPE"] = "read-only"
        self.proc = subprocess.Popen(
            [server_cmd, vault],
            text=True,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        self.next_id = 0
        self._rpc(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "vault-digest", "version": "1.0"},
            },
        )
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def _send(self, msg):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def _rpc(self, method, params):
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        for line in self.proc.stdout:
            msg = json.loads(line)
            if msg.get("id") == self.next_id:
                return msg["result"]
        raise RuntimeError("server closed the stream")

    def call(self, tool, args=None):
        """Call a tool; raise a readable error on {ok: false}."""
        result = self._rpc("tools/call", {"name": tool, "arguments": args or {}})
        payload = json.loads(result["content"][0]["text"])
        if not payload["ok"]:
            e = payload["error"]
            raise RuntimeError(f"{e['code']}: {e['reason']} — {e['suggestion']}")
        return payload["data"]

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=5)


ORPHAN_SCAN_CAP = 100  # graph_query is 2 calls/note; keep examples snappy


def section(title):
    print(f"\n\033[1m{title}\033[0m")


def main():
    parser = argparse.ArgumentParser(description="Print a Tacitus vault health digest.")
    parser.add_argument("vault", help="path to the vault folder")
    parser.add_argument("--server", default="tacitus-mcp", help="server command (default: tacitus-mcp)")
    args = parser.parse_args()

    t = Tacitus(args.server, args.vault, read_only=True)
    try:
        caps = t.call("capabilities")
        print(f"Tacitus {caps['version']} · scope: {caps['permissions']['scope']} · vault: {args.vault}")

        notes = t.call("list_notes")["notes"]
        today = datetime.date.today().isoformat()

        # --- tasks by urgency -------------------------------------------------
        tasks = t.call("list_tasks", {"done": False, "limit": 500})["tasks"]
        overdue = [x for x in tasks if x["due"] and x["due"] < today]
        due_today = [x for x in tasks if x["due"] == today]
        upcoming = [x for x in tasks if x["due"] and x["due"] > today]
        undated = [x for x in tasks if not x["due"]]

        section(f"Tasks — {len(tasks)} open")
        for label, bucket in (("OVERDUE", overdue), ("today", due_today), ("upcoming", upcoming)):
            for task in bucket[:5]:
                print(f"  [{label:8}] {task['due'] or '':10}  {task['text'][:60]}  ({task['note_id']})")
            if len(bucket) > 5:
                print(f"  [{label:8}] … and {len(bucket) - 5} more")
        if undated:
            print(f"  [no date ] {len(undated)} task(s) without a due date")

        # --- note statuses (typed frontmatter) --------------------------------
        rows = t.call(
            "properties_query",
            {"filters": [{"key": "status", "op": "exists"}], "select": ["status"], "limit": 500},
        )["rows"]
        if rows:
            section(f"Statuses — {len(rows)} notes with a status property")
            counts = Counter(str(r["properties"].get("status")) for r in rows)
            for status, count in counts.most_common():
                print(f"  {status:12} {count}")

        # --- orphan notes (no links in either direction) ----------------------
        orphans = []
        for note in notes[:ORPHAN_SCAN_CAP]:
            nid = note["note_id"]
            links = t.call("graph_query", {"from": nid, "relation": "links"})["nodes"]
            backlinks = t.call("graph_query", {"from": nid, "relation": "backlinks"})["nodes"]
            if not links and not backlinks:
                orphans.append(nid)
        section(f"Notes — {len(notes)} total")
        scanned = min(len(notes), ORPHAN_SCAN_CAP)
        print(f"  orphans (no links either way): {len(orphans)}/{scanned} scanned")
        for nid in orphans[:5]:
            print(f"    · {nid}")

        # --- recent write activity (the audit trail) --------------------------
        entries = t.call("audit_log", {"limit": 8})["entries"]
        if entries:
            section("Recent writes (audit log)")
            for e in entries:
                print(f"  {e['ts'][:16]}  {e['action']:6}  {e['version_id']}  {', '.join(e['notes'])[:50]}")
            print("  → inspect any version with get_version; undo with revert")
    finally:
        t.close()


if __name__ == "__main__":
    sys.exit(main())
