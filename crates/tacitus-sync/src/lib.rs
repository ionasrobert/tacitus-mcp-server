//! Tacitus sync — CRDT synchronization for vaults (notes + agent memories).
//!
//! Design: no file watcher — a scan-on-tick diff against a shadow state
//! (`.tacitus/sync/state.json`) detects local changes; per-note CRDT docs
//! merge them with remote updates; everything leaving the device is
//! end-to-end encrypted, the relay only ever sees ciphertext.

#[cfg(feature = "client")]
pub mod client;
pub mod crypto;
pub mod docs;
pub mod engine;
pub mod manifest;
pub mod merge;
pub mod outbox;
pub mod protocol;
pub mod scan;
pub mod state;

pub use crypto::{derive_keys, DocUpdate, Keys, SyncPayload, VaultCode};
pub use docs::{DocStore, MANIFEST_KEY};
pub use engine::{EngineEffect, Flag, SyncEngine};
pub use protocol::{ClientMsg, ServerMsg};
pub use scan::{scan, ItemKind, ScanDelta, ScanItem};
pub use state::{ItemState, ShadowState};

/// Where sync keeps its device-local state, under the vault's `.tacitus/`.
pub const SYNC_DIR: &str = ".tacitus/sync";

/// Structured, actionable errors — same philosophy as the engine's
/// `TacitusError`: an agent (or the CLI) must know what to do differently.
#[derive(Debug)]
pub struct SyncError {
    pub code: &'static str,
    pub reason: String,
}

impl SyncError {
    pub(crate) fn io(e: std::io::Error) -> Self {
        Self {
            code: "IO_ERROR",
            reason: e.to_string(),
        }
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.reason)
    }
}

impl std::error::Error for SyncError {}
