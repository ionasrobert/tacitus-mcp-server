import { readdir, readFile, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { parse as parseYaml, stringify as stringifyYaml } from 'yaml';
import { TacitusError } from '../lib/errors';
import { isPlainId } from '../lib/ids';

/**
 * Declared note types: `.tacitus/types/<name>.yaml` declares the properties a
 * note of that type carries —
 *
 *   description: An article idea
 *   properties:
 *     status: { type: string, required: true, enum: [idea, draft, published] }
 *     priority: { type: number }
 *
 * Any create/update whose resulting frontmatter has `type: <name>` for a
 * declared type is validated in NoteWriter's diff (so at propose AND commit);
 * a violation is a SCHEMA_VIOLATION listing every problem at once. Undeclared
 * types are not validated, extra properties are allowed.
 * Port of crates/tacitus-core/src/vault/schema.rs.
 */

export const PROP_KINDS = ['string', 'number', 'boolean', 'date', 'list'] as const;
export type PropKind = (typeof PROP_KINDS)[number];

export interface PropSpec {
  type: PropKind;
  required: boolean;
  enum?: unknown[];
}

export interface TypeDef {
  name: string;
  description: string | null;
  properties: Record<string, PropSpec>;
}

export type TypeEntry = TypeDef | { name: string; error: string };

const DATE_RE = /^\d{4}-\d{2}-\d{2}([T ][0-9:.]+(Z|[+-]\d{2}:?\d{2})?)?$/;

function isMapping(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function kindOf(v: unknown): string {
  if (v === null || v === undefined) return 'null';
  if (Array.isArray(v)) return 'list';
  if (typeof v === 'object') return 'mapping';
  return typeof v; // boolean | number | string
}

function matchesKind(kind: PropKind, v: unknown): boolean {
  switch (kind) {
    case 'string':
      return typeof v === 'string';
    case 'number':
      return typeof v === 'number';
    case 'boolean':
      return typeof v === 'boolean';
    case 'date':
      return typeof v === 'string' && DATE_RE.test(v);
    case 'list':
      return Array.isArray(v);
  }
}

/** A value for messages: strings quoted, everything else as YAML. */
function show(v: unknown): string {
  return typeof v === 'string' ? JSON.stringify(v) : stringifyYaml(v).trim();
}

function sameValue(a: unknown, b: unknown): boolean {
  return a === b || JSON.stringify(a) === JSON.stringify(b);
}

/** Every way `frontmatter` falls short of `def`, in property order (null = absent). */
export function violations(def: TypeDef, frontmatter: Record<string, unknown>): string[] {
  const out: string[] = [];
  for (const key of Object.keys(def.properties).sort()) {
    const spec = def.properties[key]!;
    const value = frontmatter[key];
    const k = JSON.stringify(key);
    if (value === null || value === undefined) {
      if (spec.required) out.push(`missing required property ${k}`);
      continue;
    }
    if (!matchesKind(spec.type, value)) {
      const hint = spec.type === 'date' ? ' (YYYY-MM-DD)' : '';
      out.push(`${k} must be ${spec.type}${hint}, got ${kindOf(value)} ${show(value)}`);
      continue;
    }
    if (spec.enum && !spec.enum.some((allowed) => sameValue(allowed, value))) {
      out.push(`${k} must be one of [${spec.enum.map(show).join(', ')}], got ${show(value)}`);
    }
  }
  return out;
}

/** Parse a type file strictly (unknown keys are errors, like serde's deny_unknown_fields). */
export function parseTypeDef(name: string, raw: string): TypeDef {
  const doc = parseYaml(raw) as unknown;
  if (!isMapping(doc)) throw new Error('expected a mapping with description/properties');
  for (const key of Object.keys(doc)) {
    if (key !== 'description' && key !== 'properties') {
      throw new Error(`unknown field \`${key}\`, expected \`description\` or \`properties\``);
    }
  }
  if (
    doc.description !== undefined &&
    doc.description !== null &&
    typeof doc.description !== 'string'
  ) {
    throw new Error('description must be a string');
  }
  const props = doc.properties ?? {};
  if (!isMapping(props)) throw new Error('properties must be a mapping');
  const properties: Record<string, PropSpec> = {};
  for (const [key, rawSpec] of Object.entries(props)) {
    if (!isMapping(rawSpec)) throw new Error(`properties.${key}: expected a mapping`);
    for (const field of Object.keys(rawSpec)) {
      if (!['type', 'required', 'enum'].includes(field)) {
        throw new Error(`properties.${key}: unknown field \`${field}\``);
      }
    }
    if (!PROP_KINDS.includes(rawSpec.type as PropKind)) {
      throw new Error(
        `properties.${key}.type: unknown variant \`${String(rawSpec.type)}\`, expected one of ${PROP_KINDS.join(', ')}`,
      );
    }
    if (rawSpec.required !== undefined && typeof rawSpec.required !== 'boolean') {
      throw new Error(`properties.${key}.required must be a boolean`);
    }
    if (rawSpec.enum !== undefined && rawSpec.enum !== null && !Array.isArray(rawSpec.enum)) {
      throw new Error(`properties.${key}.enum must be a list`);
    }
    const spec: PropSpec = { type: rawSpec.type as PropKind, required: rawSpec.required === true };
    if (Array.isArray(rawSpec.enum)) spec.enum = rawSpec.enum;
    properties[key] = spec;
  }
  return { name, description: (doc.description as string | undefined) ?? null, properties };
}

async function isFile(path: string): Promise<boolean> {
  try {
    return (await stat(path)).isFile();
  } catch {
    return false;
  }
}

/** Type declarations on disk under `.tacitus/types/*.yaml` (or `.yml`; `.yaml` wins). */
export class TypeStore {
  private readonly dir: string;

  constructor(vaultDir: string) {
    this.dir = join(vaultDir, '.tacitus', 'types');
  }

  private async file(name: string): Promise<string | null> {
    for (const ext of ['yaml', 'yml']) {
      const path = join(this.dir, `${name}.${ext}`);
      if (await isFile(path)) return path;
    }
    return null;
  }

  /** The declared type, null if undeclared; a malformed file is INVALID_TYPE_DEF. */
  async load(name: string): Promise<TypeDef | null> {
    if (!isPlainId(name)) return null;
    const path = await this.file(name);
    if (path === null) return null;
    try {
      return parseTypeDef(name, await readFile(path, 'utf8'));
    } catch (err) {
      throw new TacitusError({
        code: 'INVALID_TYPE_DEF',
        reason: `Type ${JSON.stringify(name)} (${path}) is malformed: ${err instanceof Error ? err.message : String(err)}`,
        suggestion:
          'Fix the type file: properties map to {type: string|number|boolean|date|list, required?, enum?}.',
      });
    }
  }

  /** Every type file, sorted by name; broken ones are reported, not hidden. */
  async list(): Promise<TypeEntry[]> {
    let entries: string[];
    try {
      entries = await readdir(this.dir);
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === 'ENOENT') return [];
      throw err;
    }
    const names = new Set<string>();
    for (const entry of entries) {
      const m = /^(.+)\.(yaml|yml)$/.exec(entry);
      if (m && isPlainId(m[1]!)) names.add(m[1]!);
    }
    const out: TypeEntry[] = [];
    for (const name of [...names].sort()) {
      const path = await this.file(name);
      if (path === null) continue;
      try {
        out.push(parseTypeDef(name, await readFile(path, 'utf8')));
      } catch (err) {
        out.push({ name, error: err instanceof Error ? err.message : String(err) });
      }
    }
    return out;
  }

  /**
   * Validate a note's resulting frontmatter against its declared type, if any.
   * `before` is the prior frontmatter on an update: only problems the write
   * INTRODUCES are refused, so a note that predates its type declaration can
   * still be tagged or linked — it just can't get worse.
   */
  async check(
    noteId: string,
    frontmatter: Record<string, unknown>,
    before?: Record<string, unknown>,
  ): Promise<void> {
    const typeName = frontmatter.type;
    if (typeof typeName !== 'string') return;
    const def = await this.load(typeName);
    if (def === null) return;
    const existing = new Set(before ? violations(def, before) : []);
    const problems = violations(def, frontmatter).filter((p) => !existing.has(p));
    if (problems.length === 0) return;
    throw new TacitusError({
      code: 'SCHEMA_VIOLATION',
      reason: `Note ${JSON.stringify(noteId)} (type ${JSON.stringify(typeName)}): ${problems.join('; ')}.`,
      suggestion: `Fix the frontmatter to match .tacitus/types/${typeName}.yaml (see list_types).`,
    });
  }
}

/** The list_types row for a parsed type (mirrors TypeDef::to_json in Rust). */
export function typeToJson(def: TypeDef): Record<string, unknown> {
  const properties: Record<string, unknown> = {};
  for (const key of Object.keys(def.properties).sort()) {
    const spec = def.properties[key]!;
    properties[key] = {
      type: spec.type,
      required: spec.required,
      ...(spec.enum ? { enum: spec.enum } : {}),
    };
  }
  return { name: def.name, description: def.description, properties };
}
