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
    get_note, graph_query, search_notes, ChangeOp, Changeset, HashingEmbedder, NoteFormat,
    NoteWriter, PermissionScope, Relation, SearchArgs, SearchMode, VaultIndex,
};

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
    let vault = std::env::args()
        .nth(1)
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
