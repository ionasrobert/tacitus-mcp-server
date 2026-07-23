//! Tacitus MCP server (Rust / rmcp) — a single-binary, local-first memory server.
//! Tools mirror the TS engine's memory contract: remember / recall / forget,
//! each returning a structured `{ ok, data | error }` payload as text content.

use std::path::PathBuf;

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
    get_note, graph_query, search_notes, NoteFormat, Relation, SearchArgs, VaultIndex,
};

#[derive(Clone)]
struct TacitusServer {
    vault: PathBuf,
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
        description = "Search vault notes by relevance; ranked snippets within an optional token_budget (never whole notes). Expand with get_note."
    )]
    async fn search(&self, Parameters(args): Parameters<SearchToolArgs>) -> CallToolResult {
        let index = match build_index(&self.vault) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let hits = search_notes(
            &index,
            &args.query,
            &SearchArgs {
                token_budget: args.token_budget,
                top_k: args.top_k,
            },
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let vault = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    // stdout is the protocol channel on stdio transport — log to stderr only.
    eprintln!(
        "tacitus-memory MCP server (Rust) on stdio (vault: {})",
        vault.display()
    );
    let service = TacitusServer { vault }.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
