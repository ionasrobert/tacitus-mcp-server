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

/// Test-only: collision-proof unique suffix for temp dirs. A nanosecond
/// timestamp alone can collide when parallel test threads read the clock in
/// the same instant — the atomic counter disambiguates.
#[cfg(test)]
pub(crate) fn test_unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{nanos}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}
