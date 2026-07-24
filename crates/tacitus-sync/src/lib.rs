//! Tacitus sync — CRDT synchronization for vaults (notes + agent memories).
//!
//! Design: no file watcher — a scan-on-tick diff against a shadow state
//! (`.tacitus/sync/state.json`) detects local changes; per-note CRDT docs
//! merge them with remote updates; everything leaving the device is
//! end-to-end encrypted, the relay only ever sees ciphertext.

pub mod docs;
pub mod manifest;
pub mod merge;
pub mod scan;
pub mod state;

pub use docs::{DocStore, MANIFEST_KEY};
pub use scan::{scan, ItemKind, ScanDelta, ScanItem};
pub use state::{ItemState, ShadowState};

/// Where sync keeps its device-local state, under the vault's `.tacitus/`.
pub const SYNC_DIR: &str = ".tacitus/sync";
