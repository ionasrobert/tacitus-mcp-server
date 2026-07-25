import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import type { Transport } from '@modelcontextprotocol/sdk/shared/transport.js';

import { TacitusToolError } from './errors.js';
import type {
  AuditEntry,
  Capabilities,
  ChangeOp,
  CommitResult,
  CreateFromTemplateArgs,
  GetNoteArgs,
  GetVersionArgs,
  GraphNode,
  GraphQueryArgs,
  LinkSuggestion,
  ListTasksArgs,
  NoteContent,
  NoteMeta,
  Proposal,
  PropertiesQueryArgs,
  PropertiesRow,
  RecallArgs,
  RecallResult,
  RememberArgs,
  RenameResult,
  RevertResult,
  SearchArgs,
  SearchHit,
  SuggestLinksArgs,
  TaskItem,
  TemplateMeta,
  ToggleTaskArgs,
  VersionDetail,
} from './types.js';

export interface SpawnOptions {
  /** Vault directory the server will serve. */
  vault: string;
  /**
   * Server command. Default `tacitus-mcp` (the native binary, all 25 tools).
   * For the npm server use `npx` with args `["-y", "@dashiro/tacitus-mcp-server"]`.
   */
  command?: string;
  /** Extra args placed BEFORE the vault path. */
  args?: string[];
  /**
   * Extra environment (e.g. `TACITUS_SCOPE: "read-only"` — honored by the
   * native binary only; the npm server is always read-write).
   */
  env?: Record<string, string>;
}

/**
 * Typed client for the Tacitus MCP server. Every tool is a method; every
 * failure is a thrown {@link TacitusToolError} with `{code, reason, suggestion}`.
 *
 * ```ts
 * const tacitus = await TacitusClient.spawn({ vault: '/path/to/vault' });
 * const hits = await tacitus.search({ query: 'client X', token_budget: 500 });
 * await tacitus.close();
 * ```
 */
export class TacitusClient {
  private constructor(private readonly client: Client) {}

  /** Spawn a server over stdio and connect. */
  static async spawn(options: SpawnOptions): Promise<TacitusClient> {
    const transport = new StdioClientTransport({
      command: options.command ?? 'tacitus-mcp',
      args: [...(options.args ?? []), options.vault],
      ...(options.env ? { env: { ...process.env, ...options.env } as Record<string, string> } : {}),
    });
    return TacitusClient.connect(transport);
  }

  /** Connect over any MCP transport (stdio, streamable HTTP, in-memory…). */
  static async connect(transport: Transport): Promise<TacitusClient> {
    const client = new Client({ name: 'tacitus-sdk', version: '0.1.0' });
    await client.connect(transport);
    return new TacitusClient(client);
  }

  async close(): Promise<void> {
    await this.client.close();
  }

  /**
   * Raw escape hatch: call any tool by name and get the unwrapped `data`.
   * All typed methods go through here.
   */
  async call<T>(tool: string, args: Record<string, unknown> = {}): Promise<T> {
    let result;
    try {
      result = await this.client.callTool({ name: tool, arguments: args });
    } catch (err) {
      // The server rejects tools it doesn't serve (npm = core 16) at the
      // protocol level — surface that as a structured, actionable error.
      if (err instanceof Error && /not found|unknown tool|-32602/i.test(err.message)) {
        throw new TacitusToolError(
          'TOOL_UNAVAILABLE',
          `The connected server does not serve ${tool}.`,
          'Native-first tools need the tacitus-mcp binary (GitHub Releases), not the npm server.',
        );
      }
      throw err;
    }
    const content = (result.content as { type: string; text?: string }[] | undefined)?.[0];
    if (!content || content.type !== 'text' || typeof content.text !== 'string') {
      throw new TacitusToolError(
        'INTERNAL',
        `Tool ${tool} returned no text content.`,
        'Check the server version — every Tacitus tool returns one JSON text block.',
      );
    }
    let envelope:
      | { ok: true; data: T }
      | { ok: false; error: { code: string; reason: string; suggestion: string } };
    try {
      envelope = JSON.parse(content.text);
    } catch {
      // Not our envelope — e.g. the npm server answers unknown tools with a
      // plain "MCP error -32602: tool not found" text result.
      if (/not found|unknown tool|-32602/i.test(content.text)) {
        throw new TacitusToolError(
          'TOOL_UNAVAILABLE',
          `The connected server does not serve ${tool}.`,
          'Native-first tools need the tacitus-mcp binary (GitHub Releases), not the npm server.',
        );
      }
      throw new TacitusToolError(
        'INTERNAL',
        `Tool ${tool} returned non-JSON output: ${content.text.slice(0, 120)}`,
        'Check the server version — every Tacitus tool returns one JSON envelope.',
      );
    }
    if (!envelope.ok) {
      throw new TacitusToolError(
        envelope.error.code,
        envelope.error.reason,
        envelope.error.suggestion,
      );
    }
    return envelope.data;
  }

  // ---- retrieval ----

  async search(args: SearchArgs): Promise<SearchHit[]> {
    const { hits } = await this.call<{ hits: SearchHit[] }>('search', { ...args });
    return hits;
  }

  async getNote(args: GetNoteArgs): Promise<NoteContent> {
    return this.call<NoteContent>('get_note', { ...args });
  }

  async graphQuery(args: GraphQueryArgs): Promise<GraphNode[]> {
    const { nodes } = await this.call<{ nodes: GraphNode[] }>('graph_query', { ...args });
    return nodes;
  }

  async suggestLinks(args: SuggestLinksArgs): Promise<LinkSuggestion[]> {
    const { suggestions } = await this.call<{ suggestions: LinkSuggestion[] }>('suggest_links', {
      ...args,
    });
    return suggestions;
  }

  async listNotes(): Promise<NoteMeta[]> {
    const { notes } = await this.call<{ notes: NoteMeta[] }>('list_notes');
    return notes;
  }

  // ---- agent memory ----

  async remember(args: RememberArgs): Promise<{ memory_id: string }> {
    return this.call<{ memory_id: string }>('remember', { ...args });
  }

  async recall(args: RecallArgs): Promise<RecallResult> {
    return this.call<RecallResult>('recall', { ...args });
  }

  async forget(memoryId: string): Promise<{ removed: boolean }> {
    return this.call<{ removed: boolean }>('forget', { memory_id: memoryId });
  }

  // ---- transactional write-back ----

  async proposeChanges(ops: ChangeOp[]): Promise<Proposal> {
    return this.call<Proposal>('propose_changes', { ops });
  }

  async commitChanges(changeId: string): Promise<CommitResult> {
    return this.call<CommitResult>('commit_changes', { change_id: changeId });
  }

  async revert(versionId: string): Promise<RevertResult> {
    return this.call<RevertResult>('revert', { version_id: versionId });
  }

  async createNote(
    noteId: string,
    content: string,
    frontmatter?: Record<string, unknown>,
  ): Promise<CommitResult> {
    return this.call<CommitResult>('create_note', { note_id: noteId, content, frontmatter });
  }

  async updateNote(
    noteId: string,
    update: { content?: string; frontmatter?: Record<string, unknown> },
  ): Promise<CommitResult> {
    return this.call<CommitResult>('update_note', { note_id: noteId, ...update });
  }

  async link(from: string, to: string): Promise<CommitResult> {
    return this.call<CommitResult>('link', { from, to });
  }

  async tag(noteId: string, tag: string): Promise<CommitResult> {
    return this.call<CommitResult>('tag', { note_id: noteId, tag });
  }

  async auditLog(limit?: number): Promise<AuditEntry[]> {
    const { entries } = await this.call<{ entries: AuditEntry[] }>('audit_log', { limit });
    return entries;
  }

  // ---- properties / templates / tasks (native-first) ----

  async propertiesQuery(args: PropertiesQueryArgs = {}): Promise<PropertiesRow[]> {
    const { rows } = await this.call<{ rows: PropertiesRow[] }>('properties_query', { ...args });
    return rows;
  }

  async listTemplates(): Promise<TemplateMeta[]> {
    const { templates } = await this.call<{ templates: TemplateMeta[] }>('list_templates');
    return templates;
  }

  async createFromTemplate(
    args: CreateFromTemplateArgs,
  ): Promise<CommitResult & { note_id: string }> {
    return this.call<CommitResult & { note_id: string }>('create_from_template', { ...args });
  }

  async listTasks(args: ListTasksArgs = {}): Promise<TaskItem[]> {
    const { tasks } = await this.call<{ tasks: TaskItem[] }>('list_tasks', { ...args });
    return tasks;
  }

  async toggleTask(args: ToggleTaskArgs): Promise<CommitResult> {
    return this.call<CommitResult>('toggle_task', { ...args });
  }

  async renameNote(from: string, to: string): Promise<RenameResult> {
    return this.call<RenameResult>('rename_note', { from, to });
  }

  async deleteNote(noteId: string): Promise<CommitResult> {
    return this.call<CommitResult>('delete_note', { note_id: noteId });
  }

  async getVersion(args: GetVersionArgs): Promise<VersionDetail> {
    return this.call<VersionDetail>('get_version', { ...args });
  }

  // ---- meta ----

  /** Call this first: exactly what you can do, under which scope. */
  async capabilities(): Promise<Capabilities> {
    return this.call<Capabilities>('capabilities');
  }
}
