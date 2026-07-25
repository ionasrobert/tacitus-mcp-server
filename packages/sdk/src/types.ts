/**
 * Typed mirrors of the Tacitus MCP tool contract (docs/MCP_API.md). The
 * native binary serves all 25 tools; the npm server (`@dashiro/tacitus-mcp-server`)
 * serves the core 16 — calling a native-first tool against it throws
 * `TacitusToolError` with code `TOOL_UNAVAILABLE`.
 */

export type Scope = 'read-only' | 'read-write';
export type MemoryType = 'user' | 'feedback' | 'project' | 'reference';
export type SearchMode = 'hybrid' | 'lexical' | 'semantic';
export type NoteFormat = 'outline' | 'frontmatter_only' | 'full';
export type GraphRelation = 'links' | 'backlinks' | 'neighbors';

// ---- retrieval ----

export interface SearchArgs {
  query: string;
  mode?: SearchMode;
  /** Hard token ceiling on the returned snippets — never whole notes. */
  token_budget?: number;
  top_k?: number;
}

export interface SearchHit {
  note_id: string;
  title: string;
  score: number;
  snippet: string;
  token_count: number;
}

export interface GetNoteArgs {
  note_id: string;
  /** Progressive disclosure: outline (default) → frontmatter_only → full. */
  format?: NoteFormat;
  max_tokens?: number;
}

export interface NoteContent {
  note_id: string;
  title: string;
  format: NoteFormat;
  content: string;
  token_count: number;
  truncated: boolean;
}

export interface GraphQueryArgs {
  from: string;
  relation: GraphRelation;
  depth?: number;
}

export interface GraphNode {
  note_id: string;
  title: string;
}

export interface SuggestLinksArgs {
  note_id: string;
  top_k?: number;
  min_score?: number;
  token_budget?: number;
}

export interface LinkSuggestion {
  note_id: string;
  title: string;
  score: number;
  /** Machine-readable scoring reasons, e.g. "mentions-title", "shared-tags". */
  reasons: string[];
  snippet: string;
  token_count: number;
}

export interface NoteMeta {
  note_id: string;
  title: string;
  path: string;
}

// ---- agent memory ----

export interface Provenance {
  origin: string;
  author: 'human' | 'agent';
  /** Stamped by the server when omitted. */
  timestamp?: string;
}

export interface RememberArgs {
  content: string;
  type: MemoryType;
  tags?: string[];
  /** Conflict key, e.g. "user.timezone" — same key + different content = surfaced conflict. */
  key?: string;
  /** Mandatory: a fact without a source is not trustworthy (MISSING_PROVENANCE otherwise). */
  source: Provenance;
  ttl?: number;
}

export interface Memory {
  id: string;
  type: MemoryType;
  content: string;
  tags: string[];
  key?: string;
  source: Required<Provenance>;
  ttl?: number;
}

export interface RecallArgs {
  query: string;
  type?: MemoryType;
  token_budget?: number;
}

export interface RecallResult {
  items: { memory: Memory; score: number; token_count: number }[];
  /** Contradicting memories are surfaced, never silently resolved. */
  conflicts: { key: string; memory_ids: string[] }[];
}

// ---- transactional write-back ----

export type ChangeOp =
  | { op: 'create'; note_id: string; content?: string; frontmatter?: Record<string, unknown> }
  | { op: 'update'; note_id: string; content?: string; frontmatter?: Record<string, unknown> }
  | { op: 'delete'; note_id: string };

export interface DiffEntry {
  note_id: string;
  op: string;
  before: string | null;
  after: string | null;
}

export interface Proposal {
  change_id: string;
  diff: DiffEntry[];
}

export interface CommitResult {
  version_id: string;
}

export interface RevertResult {
  reverted: boolean;
  version_id: string;
}

export interface AuditEntry {
  ts: string;
  action: string;
  version_id: string;
  change_id?: string;
  notes: string[];
  scope: Scope;
  /** Who initiated the write when not a direct call — "sync", "plugin:<name>". */
  origin?: string;
}

// ---- properties / templates / tasks (native-first) ----

export type PropOp =
  | 'eq'
  | 'ne'
  | 'contains'
  | 'exists'
  | 'not_exists'
  | 'gt'
  | 'lt'
  | 'gte'
  | 'lte';

export interface PropertiesQueryArgs {
  filters?: { key: string; op: PropOp; value?: unknown }[];
  select?: string[];
  sort_by?: string;
  descending?: boolean;
  limit?: number;
  token_budget?: number;
}

export interface PropertiesRow {
  note_id: string;
  title: string;
  properties: Record<string, unknown>;
  token_count: number;
}

export interface TemplateMeta {
  name: string;
  vars: string[];
}

export interface CreateFromTemplateArgs {
  template: string;
  note_id: string;
  /** Scalars only; substituted before YAML parsing so numbers stay typed. */
  vars?: Record<string, string | number | boolean>;
}

export interface ListTasksArgs {
  done?: boolean;
  due_before?: string;
  due_after?: string;
  tag?: string;
  note_id?: string;
  limit?: number;
  token_budget?: number;
}

export interface TaskItem {
  note_id: string;
  line: number;
  text: string;
  done: boolean;
  due: string | null;
  tags: string[];
  token_count: number;
}

export interface ToggleTaskArgs {
  note_id: string;
  /** `line` and `text` exactly as returned by listTasks — a concurrency guard. */
  line: number;
  expect_text: string;
}

export interface RenameResult {
  version_id: string;
  from: string;
  to: string;
  links_updated_in: number;
}

export interface GetVersionArgs {
  version_id: string;
  include_content?: boolean;
  max_tokens?: number;
}

export interface VersionNoteChange {
  note_id: string;
  op: string;
  before: { content: string; truncated: boolean } | null;
  after: { content: string; truncated: boolean } | null;
}

export interface VersionDetail {
  version_id: string;
  change_id: string;
  notes: VersionNoteChange[];
}

// ---- meta ----

export interface Capabilities {
  server: string;
  version: string;
  tools: { name: string; description: string }[];
  permissions: { scope: Scope };
}
