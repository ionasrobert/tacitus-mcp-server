//! Sandboxed WASM plugin host for Tacitus.
//!
//! In Tacitus, **a plugin is an MCP client** — and this crate extends that into
//! a sandbox: a guest wasm module gets exactly one door back into the vault,
//! the `tacitus.call(tool, args)` host function, which speaks the same
//! `{ ok, data | error }` envelope as the MCP server. Capability scoping is a
//! tool allowlist declared in the plugin's `tacitus-plugin.toml`, checked at
//! load (privilege) and at call time (allowlist). No WASI, no ambient
//! filesystem: the import surface is `tacitus.call` + `tacitus.log`, nothing
//! else.
//!
//! Runaway guests are bounded by fuel (deterministic instruction budget,
//! refueled per [`PluginInstance::run`]) and a linear-memory cap — host policy
//! in [`HostConfig`], never raisable from a manifest.

pub mod abi;
pub mod host;
pub mod manifest;
pub mod registry;

pub use host::{HostConfig, PluginHost, PluginInstance};
pub use manifest::{Permissions, PluginManifest};
pub use registry::{ToolDescriptor, ToolRegistry};

use serde_json::{json, Value};
use tacitus_core::error::TacitusError;

/// The `{ ok: true, data }` envelope — same shape the MCP server returns.
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
