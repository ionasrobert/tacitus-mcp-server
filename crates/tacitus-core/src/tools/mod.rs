//! The tool layer — the single source of truth for Tacitus' agent-facing
//! contract, shared by the rmcp MCP server (`tacitus-mcp`) and the sandboxed
//! WASM plugin host (`tacitus-plugins`). One registry, one set of argument
//! shapes, one `{ ok, data | error }` envelope: `tacitus.call` in the sandbox
//! *is* `tools/call` on the wire.
//!
//! The typed per-tool methods are the rmcp server's path (no double parsing);
//! [`ToolRegistry::dispatch`] is the string-keyed path plugins use — it ALWAYS
//! returns an envelope: unknown tools, denied tools and malformed args are
//! data, never panics.

pub mod args;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::error::TacitusError;
use crate::memory::recall::RecallArgs;
use crate::memory::types::MemoryType;
use crate::memory::{MemoryStore, ProvenanceInput, RememberInput};
use crate::vault::{
    get_note, graph_query, list_tasks, parse_note, properties_query, rename_note_ops,
    render_template, search_notes, suggest_links, toggled_content, ChangeOp, Changeset, Embedder,
    HashingEmbedder, NoteFormat, NoteWriter, PermissionScope, PropFilter, PropOp,
    PropertiesQueryArgs, Relation, SearchArgs, SearchMode, SuggestArgs, TaskFilter, TemplateStore,
    VaultIndex,
};

use args::*;

/// The `{ ok: true, data }` envelope — the shape every tool returns.
pub fn ok_envelope(data: Value) -> Value {
    json!({ "ok": true, "data": data })
}

/// The `{ ok: false, error: { code, reason, suggestion } }` envelope.
pub fn err_envelope(e: &TacitusError) -> Value {
    json!({
        "ok": false,
        "error": { "code": e.code, "reason": e.reason, "suggestion": e.suggestion }
    })
}

pub struct ToolDescriptor {
    pub name: &'static str,
    /// Write tools require a read-write scope (plugin manifests are validated
    /// against this at load; the engine's `NoteWriter` enforces it at run).
    pub writes: bool,
    /// Must stay byte-identical to the `#[tool(description = ...)]` literal in
    /// tacitus-mcp (rmcp's attribute is literal-only) — the server's
    /// anti-drift test fails on any divergence.
    pub description: &'static str,
}

/// All 25 tools, in the server's definition order. `capabilities` output is
/// sorted by name (rmcp's `ToolRouter::list_all` sorts, and the wire must not
/// change).
const TOOLS: &[ToolDescriptor] = &[
    ToolDescriptor {
        name: "remember",
        writes: true,
        description: "Store a typed memory (user|feedback|project|reference) with mandatory provenance. Idempotent for identical content+source.",
    },
    ToolDescriptor {
        name: "recall",
        writes: false,
        description: "Recall memories relevant to a query, ranked, within an optional token_budget. Surfaces conflicting memories instead of silently choosing.",
    },
    ToolDescriptor {
        name: "forget",
        writes: true,
        description: "Delete a memory by id. Returns whether a memory was removed.",
    },
    ToolDescriptor {
        name: "search",
        writes: false,
        description: "Search vault notes by relevance; ranked snippets within an optional token_budget (never whole notes). mode: hybrid (default) | lexical | semantic. Expand with get_note.",
    },
    ToolDescriptor {
        name: "get_note",
        writes: false,
        description: "Fetch a note progressively: outline | frontmatter_only | full, with an optional max_tokens ceiling.",
    },
    ToolDescriptor {
        name: "graph_query",
        writes: false,
        description: "Traverse the wikilink graph: links (outgoing) | backlinks | neighbors (both directions, to depth).",
    },
    ToolDescriptor {
        name: "suggest_links",
        writes: false,
        description: "Suggest [[wikilinks]] a note is missing: ranked candidates it doesn't link to yet, scored by title mentions in the note, semantic similarity, shared tags, and existing backlinks — each with machine-readable reasons. Bounded by top_k/min_score/token_budget.",
    },
    ToolDescriptor {
        name: "list_notes",
        writes: false,
        description: "List all note ids with their titles and paths.",
    },
    ToolDescriptor {
        name: "propose_changes",
        writes: false,
        description: "Dry-run a changeset (create/update/delete notes). Returns a change_id and a before/after diff. Nothing is written until commit_changes.",
    },
    ToolDescriptor {
        name: "commit_changes",
        writes: true,
        description: "Atomically apply a previously proposed change_id (all-or-nothing). Returns a version_id usable with revert.",
    },
    ToolDescriptor {
        name: "revert",
        writes: true,
        description: "Undo a committed change by version_id, restoring the prior note state.",
    },
    ToolDescriptor {
        name: "create_note",
        writes: true,
        description: "Create a note (auto-committed). Fails if it already exists. Returns a version_id.",
    },
    ToolDescriptor {
        name: "update_note",
        writes: true,
        description: "Update a note body and/or frontmatter (auto-committed). Returns a version_id.",
    },
    ToolDescriptor {
        name: "link",
        writes: true,
        description: "Append a [[to]] wikilink to the `from` note (idempotent). Returns a version_id.",
    },
    ToolDescriptor {
        name: "tag",
        writes: true,
        description: "Add a tag to a note (deduplicated). Returns a version_id.",
    },
    ToolDescriptor {
        name: "audit_log",
        writes: false,
        description: "Read recent agent actions (commits/reverts), most recent first.",
    },
    ToolDescriptor {
        name: "properties_query",
        writes: false,
        description: "Query notes by typed frontmatter properties (Bases-like). filters are AND-ed {key, op: eq|ne|contains|exists|not_exists|gt|lt|gte|lte, value}; contains = array membership or substring; gt/lt work on numbers and ISO dates. Optional select (project keys), sort_by/descending, limit (default 50), token_budget.",
    },
    ToolDescriptor {
        name: "list_templates",
        writes: false,
        description: "List note templates (.tacitus/templates/*.md) with the vars each requires. Builtins {{date}}/{{time}}/{{datetime}} auto-fill.",
    },
    ToolDescriptor {
        name: "create_from_template",
        writes: true,
        description: "Create a note from a template (auto-committed, versioned, audited). Substitution happens before YAML parsing, so numeric vars stay typed. Returns a version_id.",
    },
    ToolDescriptor {
        name: "list_tasks",
        writes: false,
        description: "List tasks (checklist lines) across the vault as typed entities: text, done, due (from due:YYYY-MM-DD or 📅 YYYY-MM-DD), #tags. Filter by done/due_before/due_after/tag/note_id; sorted by due date; bounded by limit + token_budget.",
    },
    ToolDescriptor {
        name: "toggle_task",
        writes: true,
        description: "Flip a task's checkbox (versioned + audited). Pass the line and text exactly as returned by list_tasks — if the line changed since, you get a CONFLICT instead of toggling the wrong task.",
    },
    ToolDescriptor {
        name: "rename_note",
        writes: true,
        description: "Rename a note AND retarget every wikilink that resolves to it (alias/heading kept) — one atomic, versioned changeset; a single revert undoes the whole rename.",
    },
    ToolDescriptor {
        name: "delete_note",
        writes: true,
        description: "Delete a note (versioned + audited — revert restores it).",
    },
    ToolDescriptor {
        name: "get_version",
        writes: false,
        description: "Inspect a committed version: which notes it created/updated/deleted (default), plus each note's before/after when include_content=true (truncated to max_tokens each, default 500). Pairs with audit_log and revert.",
    },
    ToolDescriptor {
        name: "capabilities",
        writes: false,
        description: "List available tools and the current permission scope.",
    },
];

pub type EmbedderFactory = Box<dyn Fn(&Path) -> Box<dyn Embedder> + Send + Sync>;

pub struct ToolRegistry {
    vault: PathBuf,
    scope: PermissionScope,
    server_name: String,
    version: String,
    /// Shared for the registry's lifetime so a propose_changes change_id
    /// survives until commit_changes, and every write stays versioned +
    /// audited through the same transactional path as any agent.
    writer: Mutex<NoteWriter>,
    /// Invoked per search/suggest_links call (mirrors the server's historical
    /// behavior — an env-selected neural embedder probes per call and can
    /// fall back to hashing at any moment).
    embedder_factory: EmbedderFactory,
}

fn parse_args<T: serde::de::DeserializeOwned>(args: &Value) -> Result<T, TacitusError> {
    serde_json::from_value(args.clone()).map_err(|e| {
        TacitusError::new(
            "INVALID_INPUT",
            format!("Bad arguments: {e}."),
            "Match the tool's argument shape in docs/MCP_API.md.",
        )
    })
}

/// Convert a tool's JSON frontmatter into the YAML value the engine stores.
fn to_yaml(frontmatter: Option<Value>) -> Result<Option<serde_yaml::Value>, TacitusError> {
    match frontmatter {
        None => Ok(None),
        Some(json) => serde_yaml::to_value(&json).map(Some).map_err(|e| {
            TacitusError::new(
                "INVALID_INPUT",
                format!("frontmatter must be a plain object ({e})."),
                "Pass frontmatter as a JSON object of scalar/array values.",
            )
        }),
    }
}

fn to_change_op(arg: ChangeOpArg) -> Result<ChangeOp, TacitusError> {
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
        other => Err(TacitusError::new(
            "INVALID_INPUT",
            format!("op must be create|update|delete (got {other:?})."),
            "Use a valid op.",
        )),
    }
}

fn internal(e: impl std::fmt::Display, suggestion: &str) -> TacitusError {
    TacitusError::new("INTERNAL", e.to_string(), suggestion)
}

impl ToolRegistry {
    /// Hashing embedder, default identity. The MCP server layers on
    /// `with_identity` + `with_embedder_factory`.
    pub fn standard(vault: &Path, scope: PermissionScope) -> Self {
        Self {
            vault: vault.to_path_buf(),
            scope,
            server_name: "tacitus".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            writer: Mutex::new(NoteWriter::new(vault, scope)),
            embedder_factory: Box::new(|_| Box::new(HashingEmbedder::new())),
        }
    }

    /// The `server`/`version` the capabilities tool reports.
    pub fn with_identity(
        mut self,
        server_name: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        self.server_name = server_name.into();
        self.version = version.into();
        self
    }

    pub fn with_embedder_factory(mut self, factory: EmbedderFactory) -> Self {
        self.embedder_factory = factory;
        self
    }

    /// Replace the internal writer — the seam for embedders like the desktop
    /// app that want plugin writes reflected into a shared LIVE index
    /// (`NoteWriter::with_index`) and attributed in the audit log
    /// (`set_origin`). The writer's scope is the enforcement point, so it
    /// should match this registry's scope; `PluginHost::load_with_registry`
    /// verifies that against the manifest.
    pub fn with_writer(mut self, writer: NoteWriter) -> Self {
        self.writer = Mutex::new(writer);
        self
    }

    pub fn scope(&self) -> PermissionScope {
        self.scope
    }

    pub fn descriptors() -> &'static [ToolDescriptor] {
        TOOLS
    }

    /// Run one tool call and return the `{ ok, data | error }` envelope.
    /// `allowlist: None` = unrestricted (the server); `Some` = a plugin's
    /// manifest allowlist, enforced here.
    pub fn dispatch(&self, tool: &str, args: &Value, allowlist: Option<&HashSet<String>>) -> Value {
        if !TOOLS.iter().any(|d| d.name == tool) {
            let names: Vec<&str> = TOOLS.iter().map(|d| d.name).collect();
            return err_envelope(&TacitusError::new(
                "INVALID_INPUT",
                format!("Unknown tool {tool:?}."),
                format!("Valid tools: {}.", names.join(", ")),
            ));
        }
        if let Some(allowed) = allowlist {
            if !allowed.contains(tool) {
                return err_envelope(&TacitusError::new(
                    "PERMISSION_DENIED",
                    format!("Tool {tool:?} is not in this plugin's allowlist."),
                    "Declare it under [permissions].tools in tacitus-plugin.toml.",
                ));
            }
        }
        match self.run_tool(tool, args, allowlist) {
            Ok(data) => ok_envelope(data),
            Err(e) => err_envelope(&e),
        }
    }

    fn run_tool(
        &self,
        tool: &str,
        args: &Value,
        allowlist: Option<&HashSet<String>>,
    ) -> Result<Value, TacitusError> {
        match tool {
            "remember" => self.remember(parse_args(args)?),
            "recall" => self.recall(parse_args(args)?),
            "forget" => self.forget(parse_args(args)?),
            "search" => self.search(parse_args(args)?),
            "get_note" => self.get_note(parse_args(args)?),
            "graph_query" => self.graph_query(parse_args(args)?),
            "suggest_links" => self.suggest_links(parse_args(args)?),
            "list_notes" => self.list_notes(),
            "propose_changes" => self.propose_changes(parse_args(args)?),
            "commit_changes" => self.commit_changes(parse_args(args)?),
            "revert" => self.revert(parse_args(args)?),
            "create_note" => self.create_note(parse_args(args)?),
            "update_note" => self.update_note(parse_args(args)?),
            "link" => self.link(parse_args(args)?),
            "tag" => self.tag(parse_args(args)?),
            "audit_log" => self.audit_log(parse_args(args)?),
            "properties_query" => self.properties_query(parse_args(args)?),
            "list_templates" => self.list_templates(),
            "create_from_template" => self.create_from_template(parse_args(args)?),
            "list_tasks" => self.list_tasks(parse_args(args)?),
            "toggle_task" => self.toggle_task(parse_args(args)?),
            "rename_note" => self.rename_note(parse_args(args)?),
            "delete_note" => self.delete_note(parse_args(args)?),
            "get_version" => self.get_version(parse_args(args)?),
            "capabilities" => self.capabilities(allowlist),
            // Unreachable: dispatch() rejects unknown tools before run_tool.
            other => Err(TacitusError::new(
                "INTERNAL",
                format!("Tool {other:?} has no dispatch arm."),
                "Report this as a tacitus bug.",
            )),
        }
    }

    fn index(&self) -> Result<VaultIndex, TacitusError> {
        VaultIndex::build(&self.vault).map_err(|e| internal(e, "Check the vault path."))
    }

    // ---- memory ----

    pub fn remember(&self, args: RememberArgs) -> Result<Value, TacitusError> {
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
        let memory = crate::memory::remember(input)?;
        MemoryStore::new(&self.vault)
            .save(&memory)
            .map_err(|e| internal(e, "Check vault path permissions."))?;
        Ok(json!({ "memory_id": memory.id }))
    }

    pub fn recall(&self, args: RecallQuery) -> Result<Value, TacitusError> {
        let memories = MemoryStore::new(&self.vault)
            .load()
            .map_err(|e| internal(e, "Check the vault path."))?;
        let result = crate::memory::recall(
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
        Ok(json!({ "items": items, "conflicts": conflicts }))
    }

    pub fn forget(&self, args: ForgetArgs) -> Result<Value, TacitusError> {
        let removed = MemoryStore::new(&self.vault)
            .remove(&args.memory_id)
            .map_err(|e| internal(e, "Check the vault path."))?;
        Ok(json!({ "removed": removed }))
    }

    // ---- retrieval ----

    pub fn search(&self, args: SearchToolArgs) -> Result<Value, TacitusError> {
        let index = self.index()?;
        let mode = match args.mode.as_deref() {
            Some("lexical") => SearchMode::Lexical,
            Some("semantic") => SearchMode::Semantic,
            _ => SearchMode::Hybrid,
        };
        let embedder = (self.embedder_factory)(&self.vault);
        let hits = search_notes(
            &index,
            &args.query,
            &SearchArgs {
                mode,
                token_budget: args.token_budget,
                top_k: args.top_k,
            },
            embedder.as_ref(),
        );
        let hits: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({ "note_id": h.note_id, "title": h.title, "score": h.score, "snippet": h.snippet, "token_count": h.token_count })
            })
            .collect();
        Ok(json!({ "hits": hits }))
    }

    pub fn get_note(&self, args: GetNoteArgs) -> Result<Value, TacitusError> {
        let index = self.index()?;
        let (format, format_str) = match args.format.as_deref() {
            Some("frontmatter_only") => (NoteFormat::FrontmatterOnly, "frontmatter_only"),
            Some("full") => (NoteFormat::Full, "full"),
            _ => (NoteFormat::Outline, "outline"),
        };
        let r = get_note(&index, &args.note_id, format, args.max_tokens)?;
        Ok(json!({
            "note_id": r.note_id, "title": r.title, "format": format_str,
            "content": r.content, "token_count": r.token_count, "truncated": r.truncated
        }))
    }

    pub fn graph_query(&self, args: GraphArgs) -> Result<Value, TacitusError> {
        let index = self.index()?;
        let relation = match args.relation.as_str() {
            "links" => Relation::Links,
            "backlinks" => Relation::Backlinks,
            "neighbors" => Relation::Neighbors,
            other => {
                return Err(TacitusError::new(
                    "INVALID_INPUT",
                    format!("relation must be links|backlinks|neighbors (got {other:?})."),
                    "Use a valid relation.",
                ))
            }
        };
        let nodes = graph_query(&index, &args.from, relation, args.depth.unwrap_or(1))?;
        let nodes: Vec<Value> = nodes
            .iter()
            .map(|n| json!({ "note_id": n.note_id, "title": n.title }))
            .collect();
        Ok(json!({ "from": args.from, "relation": args.relation, "nodes": nodes }))
    }

    pub fn suggest_links(&self, args: SuggestLinksArgs) -> Result<Value, TacitusError> {
        let index = self.index()?;
        let embedder = (self.embedder_factory)(&self.vault);
        let suggestions = suggest_links(
            &index,
            &args.note_id,
            embedder.as_ref(),
            &SuggestArgs {
                top_k: args.top_k,
                min_score: args.min_score,
                token_budget: args.token_budget,
            },
        )?;
        let suggestions: Vec<Value> = suggestions
            .iter()
            .map(|s| {
                json!({ "note_id": s.note_id, "title": s.title, "score": s.score, "reasons": s.reasons, "snippet": s.snippet, "token_count": s.token_count })
            })
            .collect();
        Ok(json!({ "note_id": args.note_id, "suggestions": suggestions }))
    }

    pub fn list_notes(&self) -> Result<Value, TacitusError> {
        let index = self.index()?;
        let notes: Vec<Value> = index
            .all()
            .iter()
            .map(|n| json!({ "note_id": n.id, "title": n.title, "path": n.path }))
            .collect();
        Ok(json!({ "notes": notes }))
    }

    // ---- transactional write-back ----

    pub fn propose_changes(&self, args: ProposeArgs) -> Result<Value, TacitusError> {
        let mut ops = Vec::with_capacity(args.ops.len());
        for arg in args.ops {
            ops.push(to_change_op(arg)?);
        }
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let proposal = writer.propose(Changeset { ops })?;
        Ok(serde_json::to_value(&proposal).unwrap_or(Value::Null))
    }

    pub fn commit_changes(&self, args: CommitArgs) -> Result<Value, TacitusError> {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let result = writer.commit(&args.change_id)?;
        Ok(json!({ "version_id": result.version_id }))
    }

    pub fn revert(&self, args: RevertArgs) -> Result<Value, TacitusError> {
        let writer = self.writer.lock().expect("writer mutex poisoned");
        let result = writer.revert(&args.version_id)?;
        Ok(json!({ "reverted": result.reverted, "version_id": result.version_id }))
    }

    pub fn create_note(&self, args: CreateNoteArgs) -> Result<Value, TacitusError> {
        let frontmatter = to_yaml(args.frontmatter)?;
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let result = writer.create_note(&args.note_id, &args.content, frontmatter)?;
        Ok(json!({ "version_id": result.version_id }))
    }

    pub fn update_note(&self, args: UpdateNoteArgs) -> Result<Value, TacitusError> {
        let frontmatter = to_yaml(args.frontmatter)?;
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let result = writer.update_note(&args.note_id, args.content, frontmatter)?;
        Ok(json!({ "version_id": result.version_id }))
    }

    pub fn link(&self, args: LinkArgs) -> Result<Value, TacitusError> {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let result = writer.link(&args.from, &args.to)?;
        Ok(json!({ "version_id": result.version_id }))
    }

    pub fn tag(&self, args: TagArgs) -> Result<Value, TacitusError> {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let result = writer.tag(&args.note_id, &args.tag)?;
        Ok(json!({ "version_id": result.version_id }))
    }

    pub fn audit_log(&self, args: AuditArgs) -> Result<Value, TacitusError> {
        let writer = self.writer.lock().expect("writer mutex poisoned");
        let entries = writer.read_audit(args.limit.unwrap_or(50))?;
        Ok(json!({ "entries": serde_json::to_value(&entries).unwrap_or(Value::Null) }))
    }

    // ---- properties / templates / tasks ----

    pub fn properties_query(&self, args: PropertiesArgs) -> Result<Value, TacitusError> {
        let index = self.index()?;
        let mut filters = Vec::with_capacity(args.filters.len());
        for f in args.filters {
            let Some(op) = PropOp::parse(&f.op) else {
                return Err(TacitusError::new(
                    "INVALID_INPUT",
                    format!(
                        "op must be eq|ne|contains|exists|not_exists|gt|lt|gte|lte (got {:?}).",
                        f.op
                    ),
                    "Use a valid op.",
                ));
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
        Ok(json!({ "rows": rows }))
    }

    pub fn list_templates(&self) -> Result<Value, TacitusError> {
        let templates = TemplateStore::new(&self.vault)
            .list()
            .map_err(|e| internal(e, "Check the vault path."))?;
        let templates: Vec<Value> = templates
            .iter()
            .map(|t| json!({ "name": t.name, "vars": t.vars }))
            .collect();
        Ok(json!({ "templates": templates }))
    }

    pub fn create_from_template(
        &self,
        args: CreateFromTemplateArgs,
    ) -> Result<Value, TacitusError> {
        let raw = TemplateStore::new(&self.vault).load_raw(&args.template)?;
        let mut vars = std::collections::HashMap::new();
        if let Some(Value::Object(map)) = &args.vars {
            for (k, v) in map {
                let s = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => {
                        return Err(TacitusError::new(
                            "INVALID_INPUT",
                            format!("var {k:?} must be a scalar (string/number/bool)."),
                            "Pass scalar values in vars.",
                        ))
                    }
                };
                vars.insert(k.clone(), s);
            }
        }
        let rendered = render_template(&raw, &vars)?;
        let note = parse_note(&rendered, &format!("{}.md", args.note_id));
        let frontmatter = match &note.frontmatter {
            serde_yaml::Value::Mapping(m) if m.is_empty() => None,
            fm => Some(fm.clone()),
        };
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let result = writer.create_note(&args.note_id, &note.content, frontmatter)?;
        Ok(json!({ "version_id": result.version_id, "note_id": args.note_id }))
    }

    pub fn list_tasks(&self, args: ListTasksArgs) -> Result<Value, TacitusError> {
        let index = self.index()?;
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
        Ok(json!({ "tasks": tasks }))
    }

    pub fn toggle_task(&self, args: ToggleTaskArgs) -> Result<Value, TacitusError> {
        let index = self.index()?;
        let Some(note) = index.get(&args.note_id) else {
            return Err(TacitusError::new(
                "NOTE_NOT_FOUND",
                format!("No note with id {:?}.", args.note_id),
                "Use list_tasks to discover tasks.",
            ));
        };
        let new_content =
            toggled_content(&args.note_id, &note.content, args.line, &args.expect_text)?;
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let result = writer.update_note(&args.note_id, Some(new_content), None)?;
        Ok(json!({ "version_id": result.version_id }))
    }

    pub fn rename_note(&self, args: RenameNoteArgs) -> Result<Value, TacitusError> {
        let index = self.index()?;
        let changeset = rename_note_ops(&index, &args.from, &args.to)?;
        let updated = changeset.ops.len().saturating_sub(2); // minus create+delete
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let proposal = writer.propose(changeset)?;
        let result = writer.commit(&proposal.change_id)?;
        Ok(json!({
            "version_id": result.version_id,
            "from": args.from,
            "to": args.to,
            "links_updated_in": updated,
        }))
    }

    pub fn delete_note(&self, args: DeleteNoteArgs) -> Result<Value, TacitusError> {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let result = writer.delete_note(&args.note_id)?;
        Ok(json!({ "version_id": result.version_id }))
    }

    pub fn get_version(&self, args: GetVersionArgs) -> Result<Value, TacitusError> {
        let writer = self.writer.lock().expect("writer mutex poisoned");
        let detail = writer.get_version(&args.version_id)?;
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
        Ok(json!({
            "version_id": detail.version_id,
            "change_id": detail.change_id,
            "notes": notes,
        }))
    }

    // ---- meta ----

    /// Tools visible under the given allowlist (None = all), sorted by name —
    /// the same order rmcp's `ToolRouter::list_all` publishes on the wire.
    pub fn capabilities(&self, allowlist: Option<&HashSet<String>>) -> Result<Value, TacitusError> {
        let mut visible: Vec<&ToolDescriptor> = TOOLS
            .iter()
            .filter(|d| allowlist.is_none_or(|a| a.contains(d.name)))
            .collect();
        visible.sort_by_key(|d| d.name);
        let tools: Vec<Value> = visible
            .iter()
            .map(|d| json!({ "name": d.name, "description": d.description }))
            .collect();
        Ok(json!({
            "server": self.server_name,
            "version": self.version,
            "tools": tools,
            "permissions": { "scope": serde_json::to_value(self.scope).unwrap_or(Value::Null) },
        }))
    }
}
