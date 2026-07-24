use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// What we last observed on disk for one synced item — enough to detect
/// change cheaply: (mtime, size) fast path, content hash as the truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemState {
    pub hash: String,
    pub mtime_ms: u64,
    pub size: u64,
}

/// The device-local shadow of the vault, keyed by item key
/// (`n:<note_id>` for notes, `m:<memory_id>` for agent memories).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ShadowState {
    pub schema: u32,
    pub items: BTreeMap<String, ItemState>,
}

impl ShadowState {
    /// Load from `<sync_dir>/state.json`; a missing file is a first scan.
    pub fn load(sync_dir: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(sync_dir.join("state.json")) {
            Ok(raw) => Ok(serde_json::from_str(&raw).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Atomic save (temp + rename), creating the sync dir if needed.
    pub fn save(&self, sync_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(sync_dir)?;
        let tmp = sync_dir.join(".state.json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, sync_dir.join("state.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-sync-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn state_json_roundtrip_preserves_entries() {
        let dir = temp_dir("state");
        let mut state = ShadowState {
            schema: 1,
            ..Default::default()
        };
        state.items.insert(
            "n:projects/launch".into(),
            ItemState {
                hash: "abc123".into(),
                mtime_ms: 1_700_000_000_000,
                size: 42,
            },
        );
        state.save(&dir).unwrap();

        let loaded = ShadowState::load(&dir).unwrap();
        assert_eq!(loaded.schema, 1);
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.items["n:projects/launch"].hash, "abc123");
        assert_eq!(loaded.items["n:projects/launch"].size, 42);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_state_file_treated_as_first_scan() {
        let dir = temp_dir("state-missing");
        let state = ShadowState::load(&dir).unwrap();
        assert_eq!(state.items.len(), 0);
        fs::remove_dir_all(&dir).ok();
    }
}
