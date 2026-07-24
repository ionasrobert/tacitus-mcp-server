#!/usr/bin/env node
/**
 * Daily note — a complete, runnable Tacitus plugin (capturer / scheduled-agent
 * pattern). Run it by hand or from cron.
 *
 * What it does, all through the MCP API (versioned + audited):
 *   1. Installs its template pack on first run (.tacitus/templates/daily.md —
 *      templates are data, the safest kind of plugin payload).
 *   2. Creates today's note daily/YYYY-MM-DD from the template
 *      (CONFLICT → it already exists, we continue gracefully).
 *   3. Pulls open tasks due today or overdue and appends a "Due today"
 *      section — as plain bullets linking to the source notes, so tasks are
 *      never duplicated. Idempotent: the section is only added once.
 *
 * Usage:
 *   node examples/daily_note.mjs /path/to/vault [--focus "ship the release"]
 *   node examples/daily_note.mjs /path/to/vault --server ./target/release/tacitus-mcp
 *
 * Requires Node >= 18 and the native server (tacitus-mcp >= 0.6). Zero deps.
 */

import { spawn } from "node:child_process";
import { mkdir, writeFile, access } from "node:fs/promises";
import { join } from "node:path";

// ---------- minimal MCP stdio client ----------------------------------------

class Tacitus {
  constructor(serverCmd, vault) {
    this.proc = spawn(serverCmd, [vault], { stdio: ["pipe", "pipe", "ignore"] });
    this.nextId = 0;
    this.pending = new Map();
    let buffer = "";
    this.proc.stdout.on("data", (chunk) => {
      buffer += chunk;
      let nl;
      while ((nl = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, nl);
        buffer = buffer.slice(nl + 1);
        if (!line.trim()) continue;
        const msg = JSON.parse(line);
        const resolve = this.pending.get(msg.id);
        if (resolve) {
          this.pending.delete(msg.id);
          resolve(msg.result);
        }
      }
    });
  }

  send(msg) {
    this.proc.stdin.write(JSON.stringify(msg) + "\n");
  }

  rpc(method, params) {
    this.nextId += 1;
    const id = this.nextId;
    return new Promise((resolve) => {
      this.pending.set(id, resolve);
      this.send({ jsonrpc: "2.0", id, method, params });
    });
  }

  async init() {
    await this.rpc("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "daily-note", version: "1.0" },
    });
    this.send({ jsonrpc: "2.0", method: "notifications/initialized" });
  }

  /** Call a tool; returns data, or throws { code, message } on {ok: false}. */
  async call(tool, args = {}) {
    const result = await this.rpc("tools/call", { name: tool, arguments: args });
    const payload = JSON.parse(result.content[0].text);
    if (!payload.ok) {
      const err = new Error(`${payload.error.code}: ${payload.error.reason}`);
      err.code = payload.error.code;
      throw err;
    }
    return payload.data;
  }

  close() {
    this.proc.stdin.end();
  }
}

// ---------- the plugin -------------------------------------------------------

const TEMPLATE = `---
tags: [daily]
---
# {{date}}

Focus: {{focus}}

## Notes

`;

async function ensureTemplate(vault) {
  const dir = join(vault, ".tacitus", "templates");
  const path = join(dir, "daily.md");
  try {
    await access(path);
  } catch {
    await mkdir(dir, { recursive: true });
    await writeFile(path, TEMPLATE, "utf8");
    console.log("installed template pack: .tacitus/templates/daily.md");
  }
}

function isoDate(d) {
  return d.toISOString().slice(0, 10);
}

async function main() {
  const argv = process.argv.slice(2);
  const vault = argv.find((a) => !a.startsWith("--"));
  if (!vault) {
    console.error("usage: node daily_note.mjs /path/to/vault [--focus TEXT] [--server CMD]");
    process.exit(1);
  }
  const flag = (name, fallback) => {
    const i = argv.indexOf(`--${name}`);
    return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback;
  };
  const focus = flag("focus", "—");
  const server = flag("server", "tacitus-mcp");

  await ensureTemplate(vault);

  const t = new Tacitus(server, vault);
  await t.init();
  try {
    const today = isoDate(new Date());
    const noteId = `daily/${today}`;

    // 1. Create today's note from the template (gracefully idempotent).
    try {
      const created = await t.call("create_from_template", {
        template: "daily",
        note_id: noteId,
        vars: { focus },
      });
      console.log(`created ${noteId} (version ${created.version_id})`);
    } catch (err) {
      if (err.code !== "CONFLICT") throw err; // structured errors → precise recovery
      console.log(`${noteId} already exists — updating it`);
    }

    // 2. Pull open tasks due today or earlier.
    const tomorrow = isoDate(new Date(Date.now() + 24 * 3600 * 1000));
    const { tasks } = await t.call("list_tasks", { done: false, due_before: tomorrow });
    if (tasks.length === 0) {
      console.log("no open tasks due today — done");
      return;
    }

    // 3. Append a "Due today" section, once. Plain bullets with wikilinks to
    //    the source notes — NOT checkboxes, so tasks are never duplicated.
    const note = await t.call("get_note", { note_id: noteId, format: "full" });
    const marker = "## Due today";
    if (note.content.includes(marker)) {
      console.log(`"${marker}" section already present — done`);
      return;
    }
    const section = [
      marker,
      ...tasks.map((task) => `- ${task.text} — from [[${task.note_id}]]`),
    ].join("\n");
    const updated = await t.call("update_note", {
      note_id: noteId,
      content: `${note.content.trimEnd()}\n\n${section}\n`,
    });
    console.log(`added ${tasks.length} due task(s) (version ${updated.version_id})`);
    console.log("review with audit_log / get_version; undo with revert");
  } finally {
    t.close();
  }
}

main().catch((err) => {
  console.error(err.message);
  process.exit(1);
});
