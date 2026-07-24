//! The curated tool dispatch a sandboxed plugin can reach through
//! `tacitus.call`. Same names, same argument shapes, same `{ ok, data|error }`
//! envelope as the MCP server (docs/MCP_API.md) — `tacitus.call` *is*
//! `tools/call`. Kept to a small subset until the shared-dispatch refactor
//! (plugins-m2) lets both the rmcp server and this host serve one registry.
//!
//! `dispatch` ALWAYS returns an envelope — malformed args, unknown tools and
//! denied calls are data, never panics and never traps.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::{json, Value};
use tacitus_core::error::TacitusError;
use tacitus_core::memory::recall::RecallArgs;
use tacitus_core::memory::types::MemoryType;
use tacitus_core::memory::{recall, remember, MemoryStore, ProvenanceInput, RememberInput};
use tacitus_core::vault::{
    get_note, graph_query, list_tasks, properties_query, search_notes, HashingEmbedder, NoteFormat,
    NoteWriter, PermissionScope, PropFilter, PropOp, PropertiesQueryArgs, Relation, SearchArgs,
    SearchMode, TaskFilter, VaultIndex,
};

use crate::{err_envelope, ok_envelope};

pub struct ToolDescriptor {
    pub name: &'static str,
    /// Write tools require manifest scope = "read-write" (checked at load).
    pub writes: bool,
    pub description: &'static str,
}

/// The m1 subset. Do not grow past ~10 entries — plugins-m2 replaces this
/// with the shared dispatch used by the rmcp server.
const TOOLS: &[ToolDescriptor] = &[
    ToolDescriptor {
        name: "capabilities",
        writes: false,
        description: "List the tools this plugin may call and its permission scope.",
    },
    ToolDescriptor {
        name: "search",
        writes: false,
        description: "Search vault notes by relevance; ranked snippets within a token_budget.",
    },
    ToolDescriptor {
        name: "get_note",
        writes: false,
        description: "Fetch a note progressively: outline | frontmatter_only | full.",
    },
    ToolDescriptor {
        name: "list_notes",
        writes: false,
        description: "List all note ids with their titles and paths.",
    },
    ToolDescriptor {
        name: "graph_query",
        writes: false,
        description: "Traverse the wikilink graph: links | backlinks | neighbors.",
    },
    ToolDescriptor {
        name: "list_tasks",
        writes: false,
        description: "List checklist tasks as typed entities (done/due/tags), bounded.",
    },
    ToolDescriptor {
        name: "properties_query",
        writes: false,
        description: "Query notes by typed frontmatter properties (Bases-like).",
    },
    ToolDescriptor {
        name: "recall",
        writes: false,
        description: "Recall agent memories relevant to a query, ranked, with conflicts surfaced.",
    },
    ToolDescriptor {
        name: "create_note",
        writes: true,
        description: "Create a note (auto-committed, versioned, audited).",
    },
    ToolDescriptor {
        name: "update_note",
        writes: true,
        description: "Update a note body and/or frontmatter (auto-committed, versioned, audited).",
    },
    ToolDescriptor {
        name: "remember",
        writes: true,
        description: "Store a typed agent memory with mandatory provenance.",
    },
];

pub struct ToolRegistry {
    vault: PathBuf,
    scope: PermissionScope,
    /// Shared for the registry's lifetime — writes stay versioned + audited
    /// through the same transactional path as every other agent.
    writer: Mutex<NoteWriter>,
    embedder: HashingEmbedder,
}

// ---- arg shapes: field-for-field mirrors of crates/tacitus-mcp/src/main.rs ----

#[derive(Deserialize)]
struct SourceArg {
    origin: String,
    author: String,
    timestamp: Option<String>,
}

#[derive(Deserialize)]
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

#[derive(Deserialize)]
struct RecallQuery {
    query: String,
    #[serde(rename = "type")]
    memory_type: Option<String>,
    token_budget: Option<usize>,
}

#[derive(Deserialize)]
struct SearchToolArgs {
    query: String,
    mode: Option<String>,
    token_budget: Option<usize>,
    top_k: Option<usize>,
}

#[derive(Deserialize)]
struct GetNoteArgs {
    note_id: String,
    format: Option<String>,
    max_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct GraphArgs {
    from: String,
    relation: String,
    depth: Option<usize>,
}

#[derive(Deserialize)]
struct ListTasksArgs {
    done: Option<bool>,
    due_before: Option<String>,
    due_after: Option<String>,
    tag: Option<String>,
    note_id: Option<String>,
    limit: Option<usize>,
    token_budget: Option<usize>,
}

#[derive(Deserialize)]
struct PropFilterArg {
    key: String,
    op: String,
    value: Option<Value>,
}

#[derive(Deserialize)]
struct PropertiesArgs {
    #[serde(default)]
    filters: Vec<PropFilterArg>,
    select: Option<Vec<String>>,
    sort_by: Option<String>,
    descending: Option<bool>,
    limit: Option<usize>,
    token_budget: Option<usize>,
}

#[derive(Deserialize)]
struct CreateNoteArgs {
    note_id: String,
    content: String,
    frontmatter: Option<Value>,
}

#[derive(Deserialize)]
struct UpdateNoteArgs {
    note_id: String,
    content: Option<String>,
    frontmatter: Option<Value>,
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

impl ToolRegistry {
    pub fn standard(vault: &Path, scope: PermissionScope) -> Self {
        Self {
            vault: vault.to_path_buf(),
            scope,
            writer: Mutex::new(NoteWriter::new(vault, scope)),
            embedder: HashingEmbedder::new(),
        }
    }

    pub fn descriptors() -> &'static [ToolDescriptor] {
        TOOLS
    }

    /// Run one tool call and return the `{ ok, data | error }` envelope.
    /// `allowlist: None` = unrestricted (host-internal use); `Some` = the
    /// plugin's manifest allowlist, enforced here.
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

    fn index(&self) -> Result<VaultIndex, TacitusError> {
        Ok(VaultIndex::build(&self.vault)?)
    }

    fn run_tool(
        &self,
        tool: &str,
        args: &Value,
        allowlist: Option<&HashSet<String>>,
    ) -> Result<Value, TacitusError> {
        match tool {
            "capabilities" => {
                let tools: Vec<Value> = TOOLS
                    .iter()
                    .filter(|d| allowlist.is_none_or(|a| a.contains(d.name)))
                    .map(|d| json!({ "name": d.name, "description": d.description }))
                    .collect();
                Ok(json!({
                    "server": "tacitus-plugins",
                    "version": env!("CARGO_PKG_VERSION"),
                    "tools": tools,
                    "permissions": { "scope": serde_json::to_value(self.scope).unwrap_or(Value::Null) },
                }))
            }
            "search" => {
                let args: SearchToolArgs = parse_args(args)?;
                let index = self.index()?;
                let mode = match args.mode.as_deref() {
                    Some("lexical") => SearchMode::Lexical,
                    Some("semantic") => SearchMode::Semantic,
                    _ => SearchMode::Hybrid,
                };
                let hits = search_notes(
                    &index,
                    &args.query,
                    &SearchArgs {
                        mode,
                        token_budget: args.token_budget,
                        top_k: args.top_k,
                    },
                    &self.embedder,
                );
                let hits: Vec<Value> = hits
                    .iter()
                    .map(|h| {
                        json!({ "note_id": h.note_id, "title": h.title, "score": h.score, "snippet": h.snippet, "token_count": h.token_count })
                    })
                    .collect();
                Ok(json!({ "hits": hits }))
            }
            "get_note" => {
                let args: GetNoteArgs = parse_args(args)?;
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
            "list_notes" => {
                let index = self.index()?;
                let notes: Vec<Value> = index
                    .all()
                    .iter()
                    .map(|n| json!({ "note_id": n.id, "title": n.title, "path": n.path }))
                    .collect();
                Ok(json!({ "notes": notes }))
            }
            "graph_query" => {
                let args: GraphArgs = parse_args(args)?;
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
            "list_tasks" => {
                let args: ListTasksArgs = parse_args(args)?;
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
            "properties_query" => {
                let args: PropertiesArgs = parse_args(args)?;
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
            "recall" => {
                let args: RecallQuery = parse_args(args)?;
                let memories = MemoryStore::new(&self.vault).load()?;
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
                Ok(json!({ "items": items, "conflicts": conflicts }))
            }
            "remember" => {
                let args: RememberArgs = parse_args(args)?;
                let memory = remember(RememberInput {
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
                })?;
                MemoryStore::new(&self.vault).save(&memory)?;
                Ok(json!({ "memory_id": memory.id }))
            }
            "create_note" => {
                let args: CreateNoteArgs = parse_args(args)?;
                let frontmatter = to_yaml(args.frontmatter)?;
                let mut writer = self.writer.lock().expect("writer mutex poisoned");
                let result = writer.create_note(&args.note_id, &args.content, frontmatter)?;
                Ok(json!({ "version_id": result.version_id }))
            }
            "update_note" => {
                let args: UpdateNoteArgs = parse_args(args)?;
                let frontmatter = to_yaml(args.frontmatter)?;
                let mut writer = self.writer.lock().expect("writer mutex poisoned");
                let result = writer.update_note(&args.note_id, args.content, frontmatter)?;
                Ok(json!({ "version_id": result.version_id }))
            }
            // Unreachable: dispatch() rejects unknown tools before run_tool.
            other => Err(TacitusError::new(
                "INTERNAL",
                format!("Tool {other:?} has no dispatch arm."),
                "Report this as a tacitus-plugins bug.",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_vault(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tacitus-plugins-registry-{tag}-{nanos}"));
        fs::create_dir_all(dir.join("notes")).unwrap();
        fs::write(
            dir.join("notes/alpha.md"),
            "# Alpha\n\nLaunch checklist for the alpha release.\n",
        )
        .unwrap();
        fs::write(
            dir.join("notes/beta.md"),
            "# Beta\n\nNotes about the beta program. See [[notes/alpha]].\n",
        )
        .unwrap();
        dir
    }

    fn allow(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dispatch_search_returns_hits_envelope() {
        let vault = temp_vault("search");
        let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
        let out = reg.dispatch("search", &json!({ "query": "alpha launch" }), None);
        assert_eq!(out["ok"], true);
        let hits = out["data"]["hits"].as_array().unwrap();
        assert!(!hits.is_empty());
        let hit = &hits[0];
        for key in ["note_id", "title", "score", "snippet", "token_count"] {
            assert!(hit.get(key).is_some(), "hit has {key}");
        }
    }

    #[test]
    fn dispatch_unknown_tool_is_invalid_input() {
        let vault = temp_vault("unknown");
        let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
        let out = reg.dispatch("no_such_tool", &json!({}), None);
        assert_eq!(out["ok"], false);
        assert_eq!(out["error"]["code"], "INVALID_INPUT");
        assert!(out["error"]["suggestion"]
            .as_str()
            .unwrap()
            .contains("search"));
    }

    #[test]
    fn dispatch_denies_tool_missing_from_allowlist() {
        let vault = temp_vault("denied");
        let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
        let allowed = allow(&["get_note"]);
        let out = reg.dispatch("search", &json!({ "query": "alpha" }), Some(&allowed));
        assert_eq!(out["ok"], false);
        assert_eq!(out["error"]["code"], "PERMISSION_DENIED");
        assert!(out["error"]["suggestion"]
            .as_str()
            .unwrap()
            .contains("tacitus-plugin.toml"));
    }

    #[test]
    fn dispatch_create_note_readonly_scope_denied() {
        let vault = temp_vault("readonly");
        let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
        let out = reg.dispatch(
            "create_note",
            &json!({ "note_id": "notes/x", "content": "hi" }),
            None,
        );
        assert_eq!(out["ok"], false);
        assert_eq!(out["error"]["code"], "PERMISSION_DENIED");
    }

    #[test]
    fn dispatch_create_then_get_note_roundtrip() {
        let vault = temp_vault("roundtrip");
        let reg = ToolRegistry::standard(&vault, PermissionScope::ReadWrite);
        let out = reg.dispatch(
            "create_note",
            &json!({ "note_id": "notes/fresh", "content": "hello from a plugin" }),
            None,
        );
        assert_eq!(out["ok"], true, "create failed: {out}");
        assert!(out["data"]["version_id"].is_string());

        let out = reg.dispatch(
            "get_note",
            &json!({ "note_id": "notes/fresh", "format": "full" }),
            None,
        );
        assert_eq!(out["ok"], true);
        assert!(out["data"]["content"]
            .as_str()
            .unwrap()
            .contains("hello from a plugin"));
    }

    #[test]
    fn dispatch_remember_then_recall_roundtrip() {
        let vault = temp_vault("memory");
        let reg = ToolRegistry::standard(&vault, PermissionScope::ReadWrite);
        let out = reg.dispatch(
            "remember",
            &json!({
                "content": "The plugin marketplace launches in autumn.",
                "type": "project",
                "source": { "origin": "test", "author": "agent" }
            }),
            None,
        );
        assert_eq!(out["ok"], true, "remember failed: {out}");
        assert!(out["data"]["memory_id"].is_string());

        let out = reg.dispatch("recall", &json!({ "query": "marketplace launch" }), None);
        assert_eq!(out["ok"], true);
        assert!(!out["data"]["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn dispatch_capabilities_reflects_allowlist_only() {
        let vault = temp_vault("caps");
        let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
        let allowed = allow(&["capabilities", "search"]);
        let out = reg.dispatch("capabilities", &json!({}), Some(&allowed));
        assert_eq!(out["ok"], true);
        let names: Vec<&str> = out["data"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["capabilities", "search"],
            "allowlist only, not all tools"
        );
        assert_eq!(out["data"]["permissions"]["scope"], "read-only");
    }

    #[test]
    fn dispatch_never_panics_on_malformed_args() {
        let vault = temp_vault("malformed");
        let reg = ToolRegistry::standard(&vault, PermissionScope::ReadOnly);
        let out = reg.dispatch("search", &json!({ "query": 42 }), None);
        assert_eq!(out["ok"], false);
        assert_eq!(out["error"]["code"], "INVALID_INPUT");
        let out = reg.dispatch("get_note", &json!("not an object"), None);
        assert_eq!(out["ok"], false);
        assert_eq!(out["error"]["code"], "INVALID_INPUT");
    }
}
