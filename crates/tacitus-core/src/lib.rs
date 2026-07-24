//! Core engine for Tacitus — the Rust port of the agent-native memory/retrieval
//! engine. Designed for a single-binary, local-first MCP server (via `rmcp`).
//!
//! Ports the TypeScript reference in `packages/mcp-server`. `stable_id` uses the
//! same sha256 seed format, so memory ids are identical across both engines.

pub mod error;
pub mod ids;
pub mod lexical;
pub mod memory;
pub mod tokens;
pub mod tools;
pub mod vault;
