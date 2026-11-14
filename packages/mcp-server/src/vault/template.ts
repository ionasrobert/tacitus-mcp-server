import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { TacitusError } from '../lib/errors';

/**
 * Templates with schema: a template is a Markdown file in
 * `.tacitus/templates/` whose `{{var}}` placeholders form its schema.
 * Rendering substitutes on the RAW text — frontmatter included — so
 * `priority: {{p}}` with p=3 parses back as a real YAML number. Builtins
 * `{{date}}`, `{{time}}`, `{{datetime}}` auto-fill; anything else missing is a
 * structured MISSING_VARS error naming exactly what to supply.
 * Port of crates/tacitus-core/src/vault/template.rs.
 */

const BUILTINS = new Set(['date', 'time', 'datetime']);
const PLACEHOLDER_RE = /\{\{\s*([A-Za-z0-9_]+)\s*\}\}/g;

export interface Template {
  name: string;
  /** Variables an agent must supply (builtins excluded). */
  vars: string[];
}

/** Unique non-builtin placeholders, in order of first appearance. */
export function templateVars(raw: string): string[] {
  const vars: string[] = [];
  for (const m of raw.matchAll(PLACEHOLDER_RE)) {
    const name = m[1] ?? '';
    if (!BUILTINS.has(name) && !vars.includes(name)) vars.push(name);
  }
  return vars;
}

const pad = (n: number): string => String(n).padStart(2, '0');

/** Local-time builtins, formatted like chrono's `%Y-%m-%d`, `%H:%M` and `to_rfc3339()`. */
export function builtinValue(name: string, now: Date = new Date()): string {
  const date = `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
  const time = `${pad(now.getHours())}:${pad(now.getMinutes())}`;
  if (name === 'date') return date;
  if (name === 'time') return time;
  const offset = -now.getTimezoneOffset();
  const sign = offset >= 0 ? '+' : '-';
  const abs = Math.abs(offset);
  const ms = String(now.getMilliseconds()).padStart(3, '0');
  return `${date}T${time}:${pad(now.getSeconds())}.${ms}${sign}${pad(Math.floor(abs / 60))}:${pad(abs % 60)}`;
}

/**
 * Substitute `{{var}}` placeholders. Caller-supplied vars win over builtins;
 * any other unresolved placeholder is a MISSING_VARS error listing all of them,
 * so an agent can fix the call in one retry.
 */
export function renderTemplate(
  raw: string,
  vars: Record<string, string>,
  now = new Date(),
): string {
  const missing = templateVars(raw).filter((v) => !Object.prototype.hasOwnProperty.call(vars, v));
  if (missing.length > 0) {
    throw new TacitusError({
      code: 'MISSING_VARS',
      reason: `Template requires vars: ${missing.join(', ')}.`,
      suggestion: 'Pass every listed var in the vars object.',
    });
  }
  return raw.replace(PLACEHOLDER_RE, (_whole, name: string) =>
    Object.prototype.hasOwnProperty.call(vars, name) ? (vars[name] ?? '') : builtinValue(name, now),
  );
}

/** Templates on disk under `.tacitus/templates/*.md`. */
export class TemplateStore {
  private readonly dir: string;

  constructor(vaultDir: string) {
    this.dir = join(vaultDir, '.tacitus', 'templates');
  }

  /** All templates, sorted by name, each with its inferred vars. */
  async list(): Promise<Template[]> {
    let entries: string[];
    try {
      entries = await readdir(this.dir);
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === 'ENOENT') return [];
      throw err;
    }
    const templates: Template[] = [];
    for (const entry of entries) {
      if (!entry.endsWith('.md') || entry === '.md') continue;
      try {
        const raw = await readFile(join(this.dir, entry), 'utf8');
        templates.push({ name: entry.slice(0, -3), vars: templateVars(raw) });
      } catch {
        // Unreadable entry (e.g. a directory named x.md) — skip, like the Rust store.
      }
    }
    return templates.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  }

  /** Raw template contents; the name must be a bare file stem. */
  async loadRaw(name: string): Promise<string> {
    if (name === '' || name.includes('/') || name.includes('\\') || name.includes('..')) {
      throw new TacitusError({
        code: 'TEMPLATE_NOT_FOUND',
        reason: `Invalid template name ${JSON.stringify(name)}.`,
        suggestion: 'Use a bare template name from list_templates.',
      });
    }
    try {
      return await readFile(join(this.dir, `${name}.md`), 'utf8');
    } catch {
      throw new TacitusError({
        code: 'TEMPLATE_NOT_FOUND',
        reason: `No template named ${JSON.stringify(name)}.`,
        suggestion: 'Use list_templates to see available templates.',
      });
    }
  }
}
