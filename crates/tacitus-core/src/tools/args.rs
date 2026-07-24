//! Argument shapes for every tool — the single source of truth shared by the
//! rmcp server (which also derives JSON schemas from them, feature "schemas")
//! and the WASM plugin host (which deserializes them from `tacitus.call`
//! JSON). Field doc comments become schema descriptions on the MCP wire —
//! change them only deliberately.

use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct SourceArg {
    pub origin: String,
    pub author: String,
    pub timestamp: Option<String>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct RememberArgs {
    pub content: String,
    #[serde(rename = "type")]
    pub memory_type: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub key: Option<String>,
    pub source: Option<SourceArg>,
    pub ttl: Option<u64>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct RecallQuery {
    pub query: String,
    #[serde(rename = "type")]
    pub memory_type: Option<String>,
    pub token_budget: Option<usize>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct ForgetArgs {
    pub memory_id: String,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct SearchToolArgs {
    pub query: String,
    pub mode: Option<String>,
    pub token_budget: Option<usize>,
    pub top_k: Option<usize>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct GetNoteArgs {
    pub note_id: String,
    pub format: Option<String>,
    pub max_tokens: Option<usize>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct GraphArgs {
    pub from: String,
    pub relation: String,
    pub depth: Option<usize>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct SuggestLinksArgs {
    pub note_id: String,
    /// Max suggestions to return (default 5).
    pub top_k: Option<usize>,
    /// Minimum blended score 0..1 to include (default 0.15).
    pub min_score: Option<f32>,
    pub token_budget: Option<usize>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct ChangeOpArg {
    pub op: String,
    pub note_id: String,
    pub content: Option<String>,
    pub frontmatter: Option<Value>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct ProposeArgs {
    pub ops: Vec<ChangeOpArg>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct CommitArgs {
    pub change_id: String,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct RevertArgs {
    pub version_id: String,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct CreateNoteArgs {
    pub note_id: String,
    pub content: String,
    pub frontmatter: Option<Value>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct UpdateNoteArgs {
    pub note_id: String,
    pub content: Option<String>,
    pub frontmatter: Option<Value>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct LinkArgs {
    pub from: String,
    pub to: String,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct TagArgs {
    pub note_id: String,
    pub tag: String,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct AuditArgs {
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct PropFilterArg {
    pub key: String,
    /// eq | ne | contains | exists | not_exists | gt | lt | gte | lte
    pub op: String,
    pub value: Option<Value>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct PropertiesArgs {
    #[serde(default)]
    pub filters: Vec<PropFilterArg>,
    pub select: Option<Vec<String>>,
    pub sort_by: Option<String>,
    pub descending: Option<bool>,
    pub limit: Option<usize>,
    pub token_budget: Option<usize>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct CreateFromTemplateArgs {
    /// A template name from list_templates.
    pub template: String,
    pub note_id: String,
    /// Placeholder values, e.g. {"title": "Standup", "p": 3}. Scalars only.
    pub vars: Option<Value>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct ListTasksArgs {
    /// false = open tasks, true = done, omitted = all.
    pub done: Option<bool>,
    /// Only tasks due strictly before this ISO date (YYYY-MM-DD).
    pub due_before: Option<String>,
    /// Only tasks due on/after this ISO date.
    pub due_after: Option<String>,
    pub tag: Option<String>,
    pub note_id: Option<String>,
    pub limit: Option<usize>,
    pub token_budget: Option<usize>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct ToggleTaskArgs {
    pub note_id: String,
    /// The task's `line` as returned by list_tasks.
    pub line: usize,
    /// The task's `text` as returned by list_tasks — a concurrency guard.
    pub expect_text: String,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct RenameNoteArgs {
    pub from: String,
    pub to: String,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct GetVersionArgs {
    /// A version_id from audit_log / commit_changes.
    pub version_id: String,
    /// Include each note's before/after contents (default: false = ops only).
    pub include_content: Option<bool>,
    /// Approximate token ceiling per included content (default 500).
    pub max_tokens: Option<usize>,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
pub struct DeleteNoteArgs {
    pub note_id: String,
}
