//! Tacitus MCP server (Rust / rmcp) — a single-binary, local-first memory server.
//! The tool contract lives in `tacitus_core::tools::ToolRegistry` — the single
//! source of truth shared with the sandboxed WASM plugin host. This file is
//! the rmcp skin over it: schema publication (via the shared arg structs,
//! feature "schemas") and one-line shims per tool. Descriptions are duplicated
//! in the `#[tool]` attributes because rmcp's macro is literal-only — the
//! `router_matches_registry_descriptors` test pins them to the registry.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::transport::stdio;
use rmcp::{tool, tool_router, ServiceExt};
use serde_json::{json, Value};

use tacitus_core::error::TacitusError;
use tacitus_core::tools::args::{
    AuditArgs, CommitArgs, CreateFromTemplateArgs, CreateNoteArgs, DeleteNoteArgs, ForgetArgs,
    GetNoteArgs, GetVersionArgs, GraphArgs, LinkArgs, ListTasksArgs, PropertiesArgs, ProposeArgs,
    RecallQuery, RememberArgs, RenameNoteArgs, RevertArgs, SearchToolArgs, SuggestLinksArgs,
    TagArgs, ToggleTaskArgs, UpdateNoteArgs,
};
use tacitus_core::tools::ToolRegistry;
use tacitus_core::vault::{HashingEmbedder, PermissionScope};

mod sync_cli;

/// The embedder for search/suggest tools. Default: the deterministic
/// HashingEmbedder (offline, zero setup). `TACITUS_EMBEDDER=ollama` upgrades
/// to neural embeddings via the local Ollama daemon (model from
/// `TACITUS_OLLAMA_EMBED_MODEL`, default nomic-embed-text), disk-cached under
/// .tacitus/vectors/ and falling back to hashing if the daemon is down.
fn make_embedder(vault: &std::path::Path) -> Box<dyn tacitus_core::vault::Embedder> {
    if std::env::var("TACITUS_EMBEDDER").as_deref() == Ok("ollama") {
        let url = std::env::var("TACITUS_OLLAMA_URL")
            .unwrap_or_else(|_| tacitus_core::vault::embed_ollama::DEFAULT_URL.into());
        let model = std::env::var("TACITUS_OLLAMA_EMBED_MODEL")
            .unwrap_or_else(|_| tacitus_core::vault::embed_ollama::DEFAULT_MODEL.into());
        match tacitus_core::vault::OllamaEmbedder::probe(&url, &model) {
            Ok(embedder) => {
                let cache = vault.join(".tacitus").join("vectors").join("ollama.json");
                return Box::new(tacitus_core::vault::CachedEmbedder::new(embedder, cache));
            }
            Err(e) => {
                eprintln!("TACITUS_EMBEDDER=ollama unavailable ({e}); using hashing embedder")
            }
        }
    }
    Box::new(HashingEmbedder::new())
}

#[derive(Clone)]
struct TacitusServer {
    registry: Arc<ToolRegistry>,
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

fn envelope(result: Result<Value, TacitusError>) -> CallToolResult {
    match result {
        Ok(data) => ok(data),
        Err(e) => err(&e.code, e.reason, &e.suggestion),
    }
}

#[tool_router(server_handler)]
impl TacitusServer {
    #[tool(
        description = "Store a typed memory (user|feedback|project|reference) with mandatory provenance. Idempotent for identical content+source."
    )]
    async fn remember(&self, Parameters(args): Parameters<RememberArgs>) -> CallToolResult {
        envelope(self.registry.remember(args))
    }

    #[tool(
        description = "Recall memories relevant to a query, ranked, within an optional token_budget. Surfaces conflicting memories instead of silently choosing."
    )]
    async fn recall(&self, Parameters(args): Parameters<RecallQuery>) -> CallToolResult {
        envelope(self.registry.recall(args))
    }

    #[tool(description = "Delete a memory by id. Returns whether a memory was removed.")]
    async fn forget(&self, Parameters(args): Parameters<ForgetArgs>) -> CallToolResult {
        envelope(self.registry.forget(args))
    }

    #[tool(
        description = "Search vault notes by relevance; ranked snippets within an optional token_budget (never whole notes). mode: hybrid (default) | lexical | semantic. Expand with get_note."
    )]
    async fn search(&self, Parameters(args): Parameters<SearchToolArgs>) -> CallToolResult {
        envelope(self.registry.search(args))
    }

    #[tool(
        description = "Fetch a note progressively: outline | frontmatter_only | full, with an optional max_tokens ceiling."
    )]
    async fn get_note(&self, Parameters(args): Parameters<GetNoteArgs>) -> CallToolResult {
        envelope(self.registry.get_note(args))
    }

    #[tool(
        description = "Traverse the wikilink graph: links (outgoing) | backlinks | neighbors (both directions, to depth)."
    )]
    async fn graph_query(&self, Parameters(args): Parameters<GraphArgs>) -> CallToolResult {
        envelope(self.registry.graph_query(args))
    }

    #[tool(
        description = "Suggest [[wikilinks]] a note is missing: ranked candidates it doesn't link to yet, scored by title mentions in the note, semantic similarity, shared tags, and existing backlinks — each with machine-readable reasons. Bounded by top_k/min_score/token_budget."
    )]
    async fn suggest_links(
        &self,
        Parameters(args): Parameters<SuggestLinksArgs>,
    ) -> CallToolResult {
        envelope(self.registry.suggest_links(args))
    }

    #[tool(description = "List all note ids with their titles and paths.")]
    async fn list_notes(&self) -> CallToolResult {
        envelope(self.registry.list_notes())
    }

    #[tool(
        description = "Dry-run a changeset (create/update/delete notes). Returns a change_id and a before/after diff. Nothing is written until commit_changes."
    )]
    async fn propose_changes(&self, Parameters(args): Parameters<ProposeArgs>) -> CallToolResult {
        envelope(self.registry.propose_changes(args))
    }

    #[tool(
        description = "Atomically apply a previously proposed change_id (all-or-nothing). Returns a version_id usable with revert."
    )]
    async fn commit_changes(&self, Parameters(args): Parameters<CommitArgs>) -> CallToolResult {
        envelope(self.registry.commit_changes(args))
    }

    #[tool(description = "Undo a committed change by version_id, restoring the prior note state.")]
    async fn revert(&self, Parameters(args): Parameters<RevertArgs>) -> CallToolResult {
        envelope(self.registry.revert(args))
    }

    #[tool(
        description = "Create a note (auto-committed). Fails if it already exists. Returns a version_id."
    )]
    async fn create_note(&self, Parameters(args): Parameters<CreateNoteArgs>) -> CallToolResult {
        envelope(self.registry.create_note(args))
    }

    #[tool(
        description = "Update a note body and/or frontmatter (auto-committed). Returns a version_id."
    )]
    async fn update_note(&self, Parameters(args): Parameters<UpdateNoteArgs>) -> CallToolResult {
        envelope(self.registry.update_note(args))
    }

    #[tool(
        description = "Append a [[to]] wikilink to the `from` note (idempotent). Returns a version_id."
    )]
    async fn link(&self, Parameters(args): Parameters<LinkArgs>) -> CallToolResult {
        envelope(self.registry.link(args))
    }

    #[tool(description = "Add a tag to a note (deduplicated). Returns a version_id.")]
    async fn tag(&self, Parameters(args): Parameters<TagArgs>) -> CallToolResult {
        envelope(self.registry.tag(args))
    }

    #[tool(description = "Read recent agent actions (commits/reverts), most recent first.")]
    async fn audit_log(&self, Parameters(args): Parameters<AuditArgs>) -> CallToolResult {
        envelope(self.registry.audit_log(args))
    }

    #[tool(
        description = "Query notes by typed frontmatter properties (Bases-like). filters are AND-ed {key, op: eq|ne|contains|exists|not_exists|gt|lt|gte|lte, value}; contains = array membership or substring; gt/lt work on numbers and ISO dates. Optional select (project keys), sort_by/descending, limit (default 50), token_budget."
    )]
    async fn properties_query(
        &self,
        Parameters(args): Parameters<PropertiesArgs>,
    ) -> CallToolResult {
        envelope(self.registry.properties_query(args))
    }

    #[tool(
        description = "List note templates (.tacitus/templates/*.md) with the vars each requires. Builtins {{date}}/{{time}}/{{datetime}} auto-fill."
    )]
    async fn list_templates(&self) -> CallToolResult {
        envelope(self.registry.list_templates())
    }

    #[tool(
        description = "Create a note from a template (auto-committed, versioned, audited). Substitution happens before YAML parsing, so numeric vars stay typed. Returns a version_id."
    )]
    async fn create_from_template(
        &self,
        Parameters(args): Parameters<CreateFromTemplateArgs>,
    ) -> CallToolResult {
        envelope(self.registry.create_from_template(args))
    }

    #[tool(
        description = "List tasks (checklist lines) across the vault as typed entities: text, done, due (from due:YYYY-MM-DD or 📅 YYYY-MM-DD), #tags. Filter by done/due_before/due_after/tag/note_id; sorted by due date; bounded by limit + token_budget."
    )]
    async fn list_tasks(&self, Parameters(args): Parameters<ListTasksArgs>) -> CallToolResult {
        envelope(self.registry.list_tasks(args))
    }

    #[tool(
        description = "Flip a task's checkbox (versioned + audited). Pass the line and text exactly as returned by list_tasks — if the line changed since, you get a CONFLICT instead of toggling the wrong task."
    )]
    async fn toggle_task(&self, Parameters(args): Parameters<ToggleTaskArgs>) -> CallToolResult {
        envelope(self.registry.toggle_task(args))
    }

    #[tool(
        description = "Rename a note AND retarget every wikilink that resolves to it (alias/heading kept) — one atomic, versioned changeset; a single revert undoes the whole rename."
    )]
    async fn rename_note(&self, Parameters(args): Parameters<RenameNoteArgs>) -> CallToolResult {
        envelope(self.registry.rename_note(args))
    }

    #[tool(description = "Delete a note (versioned + audited — revert restores it).")]
    async fn delete_note(&self, Parameters(args): Parameters<DeleteNoteArgs>) -> CallToolResult {
        envelope(self.registry.delete_note(args))
    }

    #[tool(
        description = "Inspect a committed version: which notes it created/updated/deleted (default), plus each note's before/after when include_content=true (truncated to max_tokens each, default 500). Pairs with audit_log and revert."
    )]
    async fn get_version(&self, Parameters(args): Parameters<GetVersionArgs>) -> CallToolResult {
        envelope(self.registry.get_version(args))
    }

    #[tool(description = "List available tools and the current permission scope.")]
    async fn capabilities(&self) -> CallToolResult {
        envelope(self.registry.capabilities(None))
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
    let registry = Arc::new(
        ToolRegistry::standard(&vault, scope)
            .with_identity("tacitus-memory", env!("CARGO_PKG_VERSION"))
            .with_embedder_factory(Box::new(make_embedder)),
    );
    let service = TacitusServer { registry }.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rmcp's `#[tool]` descriptions are literal-only, so they duplicate the
    /// registry's descriptor table. This test makes drift impossible: names
    /// AND descriptions of the live router must equal the shared registry.
    #[test]
    fn router_matches_registry_descriptors() {
        let router = TacitusServer::tool_router().list_all(); // sorted by name
        let mut descriptors: Vec<_> = ToolRegistry::descriptors().iter().collect();
        descriptors.sort_by_key(|d| d.name);
        assert_eq!(router.len(), descriptors.len(), "tool count");
        for (tool, desc) in router.iter().zip(descriptors) {
            assert_eq!(tool.name.as_ref(), desc.name);
            assert_eq!(
                tool.description.as_deref(),
                Some(desc.description),
                "description drift on {:?}",
                desc.name
            );
        }
    }
}
