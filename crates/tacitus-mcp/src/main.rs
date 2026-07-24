//! Tacitus MCP server (Rust / rmcp) — a single-binary, local-first memory server.
//! Tools mirror the TS engine's contract: memory (remember / recall / forget),
//! retrieval (search / get_note / graph_query / list_notes), and transactional
//! write-back (propose_changes / commit_changes / revert + convenience helpers
//! and audit_log). Each tool returns a structured `{ ok, data | error }` payload
//! as text content.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::transport::stdio;
use rmcp::{tool, tool_router, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use tacitus_core::memory::recall::RecallArgs;
use tacitus_core::memory::types::MemoryType;
use tacitus_core::memory::{recall, remember, MemoryStore, ProvenanceInput, RememberInput};
use tacitus_core::vault::{
    get_note, graph_query, list_tasks, parse_note, properties_query, rename_note_ops,
    render_template, search_notes, suggest_links, toggled_content, ChangeOp, Changeset,
    HashingEmbedder, NoteFormat, NoteWriter, PermissionScope, PropFilter, PropOp,
    PropertiesQueryArgs, Relation, SearchArgs, SearchMode, SuggestArgs, TaskFilter, TemplateStore,
    VaultIndex,
};

mod sync_cli;

#[derive(Clone)]
struct TacitusServer {
    vault: PathBuf,
    scope: PermissionScope,
    /// Shared across tool calls so a `propose_changes` change_id survives until
    /// a later `commit_changes` (the pending changeset lives in the writer).
    writer: Arc<Mutex<NoteWriter>>,
}

#[derive(Deserialize, JsonSchema)]
struct SourceArg {
    origin: String,
    author: String,
    timestamp: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RememberArgs {
    content: String,
    #[serde(rename = "type")]
    memory_type: String,
    #[serde(default)]
    tags: Vec<String>,
    key: Option<String>,
    source: Option<SourceArg>,
    ttl: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct RecallQuery {
    query: String,
    #[serde(rename = "type")]
    memory_type: Option<String>,
    token_budget: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct ForgetArgs {
    memory_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct SearchToolArgs {
    query: String,
    mode: Option<String>,
    token_budget: Option<usize>,
    top_k: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct GetNoteArgs {
    note_id: String,
    format: Option<String>,
    max_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct GraphArgs {
    from: String,
    relation: String,
    depth: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct SuggestLinksArgs {
    note_id: String,
    /// Max suggestions to return (default 5).
    top_k: Option<usize>,
    /// Minimum blended score 0..1 to include (default 0.15).
    min_score: Option<f32>,
    token_budget: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct ChangeOpArg {
    op: String,
    note_id: String,
    content: Option<String>,
    frontmatter: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
struct ProposeArgs {
    ops: Vec<ChangeOpArg>,
}

#[derive(Deserialize, JsonSchema)]
struct CommitArgs {
    change_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct RevertArgs {
    version_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct CreateNoteArgs {
    note_id: String,
    content: String,
    frontmatter: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateNoteArgs {
    note_id: String,
    content: Option<String>,
    frontmatter: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
struct LinkArgs {
    from: String,
    to: String,
}

#[derive(Deserialize, JsonSchema)]
struct TagArgs {
    note_id: String,
    tag: String,
}

#[derive(Deserialize, JsonSchema)]
struct AuditArgs {
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct PropFilterArg {
    key: String,
    /// eq | ne | contains | exists | not_exists | gt | lt | gte | lte
    op: String,
    value: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
struct CreateFromTemplateArgs {
    /// A template name from list_templates.
    template: String,
    note_id: String,
    /// Placeholder values, e.g. {"title": "Standup", "p": 3}. Scalars only.
    vars: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
struct ListTasksArgs {
    /// false = open tasks, true = done, omitted = all.
    done: Option<bool>,
    /// Only tasks due strictly before this ISO date (YYYY-MM-DD).
    due_before: Option<String>,
    /// Only tasks due on/after this ISO date.
    due_after: Option<String>,
    tag: Option<String>,
    note_id: Option<String>,
    limit: Option<usize>,
    token_budget: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct ToggleTaskArgs {
    note_id: String,
    /// The task's `line` as returned by list_tasks.
    line: usize,
    /// The task's `text` as returned by list_tasks — a concurrency guard.
    expect_text: String,
}

#[derive(Deserialize, JsonSchema)]
struct RenameNoteArgs {
    from: String,
    to: String,
}

#[derive(Deserialize, JsonSchema)]
struct GetVersionArgs {
    /// A version_id from audit_log / commit_changes.
    version_id: String,
    /// Include each note's before/after contents (default: false = ops only).
    include_content: Option<bool>,
    /// Approximate token ceiling per included content (default 500).
    max_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct DeleteNoteArgs {
    note_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct PropertiesArgs {
    #[serde(default)]
    filters: Vec<PropFilterArg>,
    select: Option<Vec<String>>,
    sort_by: Option<String>,
    descending: Option<bool>,
    limit: Option<usize>,
    token_budget: Option<usize>,
}

fn ok(data: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        json!({ "ok": true, "data": data }).to_string(),
    )])
}

fn err(code: &str, reason: impl Into<String>, suggestion: &str) -> CallToolResult {
    let payload = json!({
        "ok": false,
        "error": { "code": code, "reason": reason.into(), "suggestion": suggestion }
    });
    CallToolResult::error(vec![ContentBlock::text(payload.to_string())])
}

fn build_index(vault: &std::path::Path) -> Result<VaultIndex, CallToolResult> {
    VaultIndex::build(vault).map_err(|e| err("INTERNAL", e.to_string(), "Check the vault path."))
}

/// Convert a tool's JSON frontmatter into the YAML value the engine stores.
fn to_yaml(frontmatter: Option<Value>) -> Result<Option<serde_yaml::Value>, CallToolResult> {
    match frontmatter {
        None => Ok(None),
        Some(json) => serde_yaml::to_value(&json).map(Some).map_err(|e| {
            err(
                "INVALID_INPUT",
                format!("frontmatter must be a plain object ({e})."),
                "Pass frontmatter as a JSON object of scalar/array values.",
            )
        }),
    }
}

fn to_change_op(arg: ChangeOpArg) -> Result<ChangeOp, CallToolResult> {
    let frontmatter = to_yaml(arg.frontmatter)?;
    match arg.op.as_str() {
        "create" => Ok(ChangeOp::Create {
            note_id: arg.note_id,
            content: arg.content.unwrap_or_default(),
            frontmatter,
        }),
        "update" => Ok(ChangeOp::Update {
            note_id: arg.note_id,
            content: arg.content,
            frontmatter,
        }),
        "delete" => Ok(ChangeOp::Delete {
            note_id: arg.note_id,
        }),
        other => Err(err(
            "INVALID_INPUT",
            format!("op must be create|update|delete (got {other:?})."),
            "Use a valid op.",
        )),
    }
}

#[tool_router(server_handler)]
impl TacitusServer {
    #[tool(
        description = "Store a typed memory (user|feedback|project|reference) with mandatory provenance. Idempotent for identical content+source."
    )]
    async fn remember(&self, Parameters(args): Parameters<RememberArgs>) -> CallToolResult {
        let input = RememberInput {
            content: args.content,
            memory_type: args.memory_type,
            tags: args.tags,
            key: args.key,
            source: args.source.map(|s| ProvenanceInput {
                origin: s.origin,
                author: s.author,
                timestamp: s.timestamp,
            }),
            ttl: args.ttl,
        };
        match remember(input) {
            Ok(memory) => match MemoryStore::new(&self.vault).save(&memory) {
                Ok(()) => ok(json!({ "memory_id": memory.id })),
                Err(e) => err("INTERNAL", e.to_string(), "Check vault path permissions."),
            },
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(
        description = "Recall memories relevant to a query, ranked, within an optional token_budget. Surfaces conflicting memories instead of silently choosing."
    )]
    async fn recall(&self, Parameters(args): Parameters<RecallQuery>) -> CallToolResult {
        let memories = match MemoryStore::new(&self.vault).load() {
            Ok(memories) => memories,
            Err(e) => return err("INTERNAL", e.to_string(), "Check the vault path."),
        };
        let result = recall(
            &memories,
            &RecallArgs {
                query: args.query,
                memory_type: args.memory_type.as_deref().and_then(MemoryType::parse),
                token_budget: args.token_budget,
            },
        );
        let items: Vec<Value> = result
            .items
            .iter()
            .map(|item| {
                json!({
                    "memory": serde_json::to_value(&item.memory).unwrap_or(Value::Null),
                    "score": item.score,
                    "token_count": item.token_count,
                })
            })
            .collect();
        let conflicts: Vec<Value> = result
            .conflicts
            .iter()
            .map(|c| json!({ "key": c.key, "memory_ids": c.memory_ids }))
            .collect();
        ok(json!({ "items": items, "conflicts": conflicts }))
    }

    #[tool(description = "Delete a memory by id. Returns whether a memory was removed.")]
    async fn forget(&self, Parameters(args): Parameters<ForgetArgs>) -> CallToolResult {
        match MemoryStore::new(&self.vault).remove(&args.memory_id) {
            Ok(removed) => ok(json!({ "removed": removed })),
            Err(e) => err("INTERNAL", e.to_string(), "Check the vault path."),
        }
    }

    #[tool(
        description = "Search vault notes by relevance; ranked snippets within an optional token_budget (never whole notes). mode: hybrid (default) | lexical | semantic. Expand with get_note."
    )]
    async fn search(&self, Parameters(args): Parameters<SearchToolArgs>) -> CallToolResult {
        let index = match build_index(&self.vault) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let mode = match args.mode.as_deref() {
            Some("lexical") => SearchMode::Lexical,
            Some("semantic") => SearchMode::Semantic,
            _ => SearchMode::Hybrid,
        };
        // Default offline embedder. A cached/neural embedder drops in here
        // behind the same Embedder trait when configured.
        let embedder = HashingEmbedder::new();
        let hits = search_notes(
            &index,
            &args.query,
            &SearchArgs {
                mode,
                token_budget: args.token_budget,
                top_k: args.top_k,
            },
            &embedder,
        );
        let hits: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({ "note_id": h.note_id, "title": h.title, "score": h.score, "snippet": h.snippet, "token_count": h.token_count })
            })
            .collect();
        ok(json!({ "hits": hits }))
    }

    #[tool(
        description = "Fetch a note progressively: outline | frontmatter_only | full, with an optional max_tokens ceiling."
    )]
    async fn get_note(&self, Parameters(args): Parameters<GetNoteArgs>) -> CallToolResult {
        let index = match build_index(&self.vault) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let (format, format_str) = match args.format.as_deref() {
            Some("frontmatter_only") => (NoteFormat::FrontmatterOnly, "frontmatter_only"),
            Some("full") => (NoteFormat::Full, "full"),
            _ => (NoteFormat::Outline, "outline"),
        };
        match get_note(&index, &args.note_id, format, args.max_tokens) {
            Ok(r) => ok(
                json!({ "note_id": r.note_id, "title": r.title, "format": format_str, "content": r.content, "token_count": r.token_count, "truncated": r.truncated }),
            ),
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(
        description = "Traverse the wikilink graph: links (outgoing) | backlinks | neighbors (both directions, to depth)."
    )]
    async fn graph_query(&self, Parameters(args): Parameters<GraphArgs>) -> CallToolResult {
        let index = match build_index(&self.vault) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let relation = match args.relation.as_str() {
            "links" => Relation::Links,
            "backlinks" => Relation::Backlinks,
            "neighbors" => Relation::Neighbors,
            other => {
                return err(
                    "INVALID_INPUT",
                    format!("relation must be links|backlinks|neighbors (got {other:?})."),
                    "Use a valid relation.",
                )
            }
        };
        match graph_query(&index, &args.from, relation, args.depth.unwrap_or(1)) {
            Ok(nodes) => {
                let nodes: Vec<Value> = nodes
                    .iter()
                    .map(|n| json!({ "note_id": n.note_id, "title": n.title }))
                    .collect();
                ok(json!({ "from": args.from, "relation": args.relation, "nodes": nodes }))
            }
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(
        description = "Suggest [[wikilinks]] a note is missing: ranked candidates it doesn't link to yet, scored by title mentions in the note, semantic similarity, shared tags, and existing backlinks — each with machine-readable reasons. Bounded by top_k/min_score/token_budget."
    )]
    async fn suggest_links(
        &self,
        Parameters(args): Parameters<SuggestLinksArgs>,
    ) -> CallToolResult {
        let index = match build_index(&self.vault) {
            Ok(i) => i,
            Err(r) => return r,
        };
        // Default offline embedder. A cached/neural embedder drops in here
        // behind the same Embedder trait when configured.
        let embedder = HashingEmbedder::new();
        match suggest_links(
            &index,
            &args.note_id,
            &embedder,
            &SuggestArgs {
                top_k: args.top_k,
                min_score: args.min_score,
                token_budget: args.token_budget,
            },
        ) {
            Ok(suggestions) => {
                let suggestions: Vec<Value> = suggestions
                    .iter()
                    .map(|s| {
                        json!({ "note_id": s.note_id, "title": s.title, "score": s.score, "reasons": s.reasons, "snippet": s.snippet, "token_count": s.token_count })
                    })
                    .collect();
                ok(json!({ "note_id": args.note_id, "suggestions": suggestions }))
            }
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(description = "List all note ids with their titles and paths.")]
    async fn list_notes(&self) -> CallToolResult {
        let index = match build_index(&self.vault) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let notes: Vec<Value> = index
            .all()
            .iter()
            .map(|n| json!({ "note_id": n.id, "title": n.title, "path": n.path }))
            .collect();
        ok(json!({ "notes": notes }))
    }

    #[tool(
        description = "Dry-run a changeset (create/update/delete notes). Returns a change_id and a before/after diff. Nothing is written until commit_changes."
    )]
    async fn propose_changes(&self, Parameters(args): Parameters<ProposeArgs>) -> CallToolResult {
        let mut ops = Vec::with_capacity(args.ops.len());
        for arg in args.ops {
            match to_change_op(arg) {
                Ok(op) => ops.push(op),
                Err(e) => return e,
            }
        }
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        match writer.propose(Changeset { ops }) {
            Ok(proposal) => ok(serde_json::to_value(&proposal).unwrap_or(Value::Null)),
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(
        description = "Atomically apply a previously proposed change_id (all-or-nothing). Returns a version_id usable with revert."
    )]
    async fn commit_changes(&self, Parameters(args): Parameters<CommitArgs>) -> CallToolResult {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        match writer.commit(&args.change_id) {
            Ok(result) => ok(json!({ "version_id": result.version_id })),
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(description = "Undo a committed change by version_id, restoring the prior note state.")]
    async fn revert(&self, Parameters(args): Parameters<RevertArgs>) -> CallToolResult {
        let writer = self.writer.lock().expect("writer mutex poisoned");
        match writer.revert(&args.version_id) {
            Ok(result) => {
                ok(json!({ "reverted": result.reverted, "version_id": result.version_id }))
            }
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(
        description = "Create a note (auto-committed). Fails if it already exists. Returns a version_id."
    )]
    async fn create_note(&self, Parameters(args): Parameters<CreateNoteArgs>) -> CallToolResult {
        let frontmatter = match to_yaml(args.frontmatter) {
            Ok(f) => f,
            Err(e) => return e,
        };
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        match writer.create_note(&args.note_id, &args.content, frontmatter) {
            Ok(result) => ok(json!({ "version_id": result.version_id })),
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(
        description = "Update a note body and/or frontmatter (auto-committed). Returns a version_id."
    )]
    async fn update_note(&self, Parameters(args): Parameters<UpdateNoteArgs>) -> CallToolResult {
        let frontmatter = match to_yaml(args.frontmatter) {
            Ok(f) => f,
            Err(e) => return e,
        };
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        match writer.update_note(&args.note_id, args.content, frontmatter) {
            Ok(result) => ok(json!({ "version_id": result.version_id })),
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(
        description = "Append a [[to]] wikilink to the `from` note (idempotent). Returns a version_id."
    )]
    async fn link(&self, Parameters(args): Parameters<LinkArgs>) -> CallToolResult {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        match writer.link(&args.from, &args.to) {
            Ok(result) => ok(json!({ "version_id": result.version_id })),
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(description = "Add a tag to a note (deduplicated). Returns a version_id.")]
    async fn tag(&self, Parameters(args): Parameters<TagArgs>) -> CallToolResult {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        match writer.tag(&args.note_id, &args.tag) {
            Ok(result) => ok(json!({ "version_id": result.version_id })),
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(description = "Read recent agent actions (commits/reverts), most recent first.")]
    async fn audit_log(&self, Parameters(args): Parameters<AuditArgs>) -> CallToolResult {
        let writer = self.writer.lock().expect("writer mutex poisoned");
        match writer.read_audit(args.limit.unwrap_or(50)) {
            Ok(entries) => {
                ok(json!({ "entries": serde_json::to_value(&entries).unwrap_or(Value::Null) }))
            }
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(
        description = "Query notes by typed frontmatter properties (Bases-like). filters are AND-ed {key, op: eq|ne|contains|exists|not_exists|gt|lt|gte|lte, value}; contains = array membership or substring; gt/lt work on numbers and ISO dates. Optional select (project keys), sort_by/descending, limit (default 50), token_budget."
    )]
    async fn properties_query(
        &self,
        Parameters(args): Parameters<PropertiesArgs>,
    ) -> CallToolResult {
        let index = match build_index(&self.vault) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let mut filters = Vec::with_capacity(args.filters.len());
        for f in args.filters {
            let Some(op) = PropOp::parse(&f.op) else {
                return err(
                    "INVALID_INPUT",
                    format!(
                        "op must be eq|ne|contains|exists|not_exists|gt|lt|gte|lte (got {:?}).",
                        f.op
                    ),
                    "Use a valid op.",
                );
            };
            filters.push(PropFilter {
                key: f.key,
                op,
                value: f.value,
            });
        }
        let rows = properties_query(
            &index,
            &PropertiesQueryArgs {
                filters,
                select: args.select,
                sort_by: args.sort_by,
                descending: args.descending.unwrap_or(false),
                limit: args.limit,
                token_budget: args.token_budget,
            },
        );
        let rows: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({ "note_id": r.note_id, "title": r.title, "properties": r.properties, "token_count": r.token_count })
            })
            .collect();
        ok(json!({ "rows": rows }))
    }

    #[tool(
        description = "List note templates (.tacitus/templates/*.md) with the vars each requires. Builtins {{date}}/{{time}}/{{datetime}} auto-fill."
    )]
    async fn list_templates(&self) -> CallToolResult {
        match TemplateStore::new(&self.vault).list() {
            Ok(templates) => {
                let templates: Vec<Value> = templates
                    .iter()
                    .map(|t| json!({ "name": t.name, "vars": t.vars }))
                    .collect();
                ok(json!({ "templates": templates }))
            }
            Err(e) => err("INTERNAL", e.to_string(), "Check the vault path."),
        }
    }

    #[tool(
        description = "Create a note from a template (auto-committed, versioned, audited). Substitution happens before YAML parsing, so numeric vars stay typed. Returns a version_id."
    )]
    async fn create_from_template(
        &self,
        Parameters(args): Parameters<CreateFromTemplateArgs>,
    ) -> CallToolResult {
        let raw = match TemplateStore::new(&self.vault).load_raw(&args.template) {
            Ok(raw) => raw,
            Err(e) => return err(&e.code, e.reason, &e.suggestion),
        };
        let mut vars = std::collections::HashMap::new();
        if let Some(Value::Object(map)) = &args.vars {
            for (k, v) in map {
                let s = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => {
                        return err(
                            "INVALID_INPUT",
                            format!("var {k:?} must be a scalar (string/number/bool)."),
                            "Pass scalar values in vars.",
                        )
                    }
                };
                vars.insert(k.clone(), s);
            }
        }
        let rendered = match render_template(&raw, &vars) {
            Ok(r) => r,
            Err(e) => return err(&e.code, e.reason, &e.suggestion),
        };
        let note = parse_note(&rendered, &format!("{}.md", args.note_id));
        let frontmatter = match &note.frontmatter {
            serde_yaml::Value::Mapping(m) if m.is_empty() => None,
            fm => Some(fm.clone()),
        };
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        match writer.create_note(&args.note_id, &note.content, frontmatter) {
            Ok(result) => ok(json!({ "version_id": result.version_id, "note_id": args.note_id })),
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(
        description = "List tasks (checklist lines) across the vault as typed entities: text, done, due (from due:YYYY-MM-DD or 📅 YYYY-MM-DD), #tags. Filter by done/due_before/due_after/tag/note_id; sorted by due date; bounded by limit + token_budget."
    )]
    async fn list_tasks(&self, Parameters(args): Parameters<ListTasksArgs>) -> CallToolResult {
        let index = match build_index(&self.vault) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let tasks = list_tasks(
            &index,
            &TaskFilter {
                done: args.done,
                due_before: args.due_before,
                due_after: args.due_after,
                tag: args.tag,
                note_id: args.note_id,
                limit: args.limit,
                token_budget: args.token_budget,
            },
        );
        let tasks: Vec<Value> = tasks
            .iter()
            .map(|t| {
                json!({ "note_id": t.note_id, "line": t.line, "text": t.text, "done": t.done, "due": t.due, "tags": t.tags, "token_count": t.token_count })
            })
            .collect();
        ok(json!({ "tasks": tasks }))
    }

    #[tool(
        description = "Flip a task's checkbox (versioned + audited). Pass the line and text exactly as returned by list_tasks — if the line changed since, you get a CONFLICT instead of toggling the wrong task."
    )]
    async fn toggle_task(&self, Parameters(args): Parameters<ToggleTaskArgs>) -> CallToolResult {
        let index = match build_index(&self.vault) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let Some(note) = index.get(&args.note_id) else {
            return err(
                "NOTE_NOT_FOUND",
                format!("No note with id {:?}.", args.note_id),
                "Use list_tasks to discover tasks.",
            );
        };
        let new_content =
            match toggled_content(&args.note_id, &note.content, args.line, &args.expect_text) {
                Ok(c) => c,
                Err(e) => return err(&e.code, e.reason, &e.suggestion),
            };
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        match writer.update_note(&args.note_id, Some(new_content), None) {
            Ok(result) => ok(json!({ "version_id": result.version_id })),
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(
        description = "Rename a note AND retarget every wikilink that resolves to it (alias/heading kept) — one atomic, versioned changeset; a single revert undoes the whole rename."
    )]
    async fn rename_note(&self, Parameters(args): Parameters<RenameNoteArgs>) -> CallToolResult {
        let index = match build_index(&self.vault) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let changeset = match rename_note_ops(&index, &args.from, &args.to) {
            Ok(cs) => cs,
            Err(e) => return err(&e.code, e.reason, &e.suggestion),
        };
        let updated = changeset.ops.len().saturating_sub(2); // minus create+delete
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let proposal = match writer.propose(changeset) {
            Ok(p) => p,
            Err(e) => return err(&e.code, e.reason, &e.suggestion),
        };
        match writer.commit(&proposal.change_id) {
            Ok(result) => ok(json!({
                "version_id": result.version_id,
                "from": args.from,
                "to": args.to,
                "links_updated_in": updated,
            })),
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(description = "Delete a note (versioned + audited — revert restores it).")]
    async fn delete_note(&self, Parameters(args): Parameters<DeleteNoteArgs>) -> CallToolResult {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        match writer.delete_note(&args.note_id) {
            Ok(result) => ok(json!({ "version_id": result.version_id })),
            Err(e) => err(&e.code, e.reason, &e.suggestion),
        }
    }

    #[tool(
        description = "Inspect a committed version: which notes it created/updated/deleted (default), plus each note's before/after when include_content=true (truncated to max_tokens each, default 500). Pairs with audit_log and revert."
    )]
    async fn get_version(&self, Parameters(args): Parameters<GetVersionArgs>) -> CallToolResult {
        let writer = self.writer.lock().expect("writer mutex poisoned");
        let detail = match writer.get_version(&args.version_id) {
            Ok(d) => d,
            Err(e) => return err(&e.code, e.reason, &e.suggestion),
        };
        let include = args.include_content.unwrap_or(false);
        let max_chars = args.max_tokens.unwrap_or(500) * 4;
        let clip = |text: &Option<String>| -> Value {
            match text {
                None => Value::Null,
                Some(t) if include => {
                    let clipped: String = t.chars().take(max_chars).collect();
                    json!({ "content": clipped, "truncated": t.chars().count() > max_chars })
                }
                Some(_) => Value::Null,
            }
        };
        let notes: Vec<Value> = detail
            .notes
            .iter()
            .map(|n| {
                json!({ "note_id": n.note_id, "op": n.op, "before": clip(&n.before), "after": clip(&n.after) })
            })
            .collect();
        ok(json!({
            "version_id": detail.version_id,
            "change_id": detail.change_id,
            "notes": notes,
        }))
    }

    #[tool(description = "List available tools and the current permission scope.")]
    async fn capabilities(&self) -> CallToolResult {
        // Introspect the live router so the list never drifts from the tools
        // actually registered (mirrors the TS capabilities tool, which lists
        // itself too).
        let tools: Vec<Value> = Self::tool_router()
            .list_all()
            .into_iter()
            .map(|t| json!({ "name": t.name, "description": t.description }))
            .collect();
        let scope = serde_json::to_value(self.scope).unwrap_or(Value::Null);
        ok(json!({
            "server": "tacitus-memory",
            "version": env!("CARGO_PKG_VERSION"),
            "tools": tools,
            "permissions": { "scope": scope },
        }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `tacitus-mcp sync …` is the sync CLI; anything else is the MCP server
    // with an optional vault path as the first argument (unchanged).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("sync") {
        if let Err(e) = sync_cli::sync_main(&args[1..]).await {
            eprintln!("sync: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let vault = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    // Minimum-privilege friendly: opt into read-only with TACITUS_SCOPE=read-only.
    let scope = match std::env::var("TACITUS_SCOPE").as_deref() {
        Ok("read-only") => PermissionScope::ReadOnly,
        _ => PermissionScope::ReadWrite,
    };
    // stdout is the protocol channel on stdio transport — log to stderr only.
    eprintln!(
        "tacitus-memory MCP server (Rust) on stdio (vault: {}, scope: {:?})",
        vault.display(),
        scope
    );
    let writer = Arc::new(Mutex::new(NoteWriter::new(&vault, scope)));
    let service = TacitusServer {
        vault,
        scope,
        writer,
    }
    .serve(stdio())
    .await?;
    service.waiting().await?;
    Ok(())
}
