use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::state::{ItemState, ShadowState};

/// What kind of synced item a key names: `n:<note_id>` or `m:<memory_id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Note,
    Memory,
}

impl ItemKind {
    pub fn of(key: &str) -> Option<ItemKind> {
        match key.split_once(':')?.0 {
            "n" => Some(ItemKind::Note),
            "m" => Some(ItemKind::Memory),
            _ => None,
        }
    }
}

/// A created/modified item, carrying its content (already read for hashing).
#[derive(Debug, Clone)]
pub struct ScanItem {
    pub key: String,
    pub content: String,
}

#[derive(Debug, Default)]
pub struct ScanDelta {
    pub created: Vec<ScanItem>,
    pub modified: Vec<ScanItem>,
    pub deleted: Vec<String>,
}

impl ScanDelta {
    pub fn is_empty(&self) -> bool {
        self.created.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

/// Diff the vault against the shadow state and update the shadow in place.
///
/// Scope: notes (`**/*.md`, skipping `.tacitus/`) plus agent memories
/// (`.tacitus/memory/*.md`). History, audit, vectors, and templates stay
/// device-local. Fast path: identical (mtime, size) skips reading the file;
/// a touched mtime with an identical hash only refreshes the shadow.
pub fn scan(vault_dir: &Path, shadow: &mut ShadowState) -> std::io::Result<ScanDelta> {
    let mut current: BTreeMap<String, PathBuf> = BTreeMap::new();

    // Notes: same walk rules as VaultIndex::build — skip `.tacitus/`, take `*.md`.
    let mut stack = vec![vault_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) != Some(".tacitus") {
                    stack.push(path);
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let rel = path
                    .strip_prefix(vault_dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let id = rel.strip_suffix(".md").unwrap_or(&rel).to_string();
                current.insert(format!("n:{id}"), path);
            }
        }
    }

    // Agent memories: flat `.tacitus/memory/*.md`, keyed by memory id (stem).
    if let Ok(entries) = fs::read_dir(vault_dir.join(".tacitus").join("memory")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    current.insert(format!("m:{stem}"), path);
                }
            }
        }
    }

    let mut delta = ScanDelta::default();

    let gone: Vec<String> = shadow
        .items
        .keys()
        .filter(|k| !current.contains_key(*k))
        .cloned()
        .collect();
    for key in gone {
        shadow.items.remove(&key);
        delta.deleted.push(key);
    }

    for (key, path) in &current {
        let Ok(meta) = fs::metadata(path) else {
            continue;
        };
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let size = meta.len();

        let prev = shadow.items.get(key);
        if let Some(prev) = prev {
            if prev.mtime_ms == mtime_ms && prev.size == size {
                continue; // fast path: nothing to read
            }
        }
        // Non-UTF-8 or unreadable files are skipped (only *.md text syncs).
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        let hash = hash_hex(&content);
        let changed = prev.map(|p| p.hash != hash);
        shadow.items.insert(
            key.clone(),
            ItemState {
                hash,
                mtime_ms,
                size,
            },
        );
        match changed {
            None => delta.created.push(ScanItem {
                key: key.clone(),
                content,
            }),
            Some(true) => delta.modified.push(ScanItem {
                key: key.clone(),
                content,
            }),
            Some(false) => {} // touched mtime, same bytes: shadow refreshed only
        }
    }
    Ok(delta)
}

fn hash_hex(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(content.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_vault(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-scan-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn keys(items: &[ScanItem]) -> Vec<&str> {
        items.iter().map(|i| i.key.as_str()).collect()
    }

    #[test]
    fn scan_empty_vault_yields_empty_delta() {
        let dir = temp_vault("empty");
        let mut shadow = ShadowState::default();
        let delta = scan(&dir, &mut shadow).unwrap();
        assert!(delta.is_empty());
        assert!(shadow.items.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_detects_created_note() {
        let dir = temp_vault("created");
        fs::create_dir_all(dir.join("projects")).unwrap();
        fs::write(dir.join("projects/launch.md"), "# Launch\n").unwrap();
        let mut shadow = ShadowState::default();

        let delta = scan(&dir, &mut shadow).unwrap();
        assert_eq!(keys(&delta.created), vec!["n:projects/launch"]);
        assert_eq!(delta.created[0].content, "# Launch\n");
        assert!(shadow.items.contains_key("n:projects/launch"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_detects_modified_note_by_hash() {
        let dir = temp_vault("modified");
        fs::write(dir.join("a.md"), "v1\n").unwrap();
        let mut shadow = ShadowState::default();
        scan(&dir, &mut shadow).unwrap();

        fs::write(dir.join("a.md"), "v2 with more text\n").unwrap();
        let delta = scan(&dir, &mut shadow).unwrap();
        assert_eq!(keys(&delta.modified), vec!["n:a"]);
        assert_eq!(delta.modified[0].content, "v2 with more text\n");
        assert!(delta.created.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_detects_deleted_note() {
        let dir = temp_vault("deleted");
        fs::write(dir.join("gone.md"), "bye\n").unwrap();
        let mut shadow = ShadowState::default();
        scan(&dir, &mut shadow).unwrap();

        fs::remove_file(dir.join("gone.md")).unwrap();
        let delta = scan(&dir, &mut shadow).unwrap();
        assert_eq!(delta.deleted, vec!["n:gone"]);
        assert!(!shadow.items.contains_key("n:gone"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_skips_tacitus_internals_but_includes_memories() {
        let dir = temp_vault("internals");
        fs::write(dir.join("note.md"), "note\n").unwrap();
        fs::create_dir_all(dir.join(".tacitus/memory")).unwrap();
        fs::create_dir_all(dir.join(".tacitus/history")).unwrap();
        fs::create_dir_all(dir.join(".tacitus/templates")).unwrap();
        fs::write(
            dir.join(".tacitus/memory/mem_ab12.md"),
            "---\nid: mem_ab12\n---\nA fact.\n",
        )
        .unwrap();
        fs::write(dir.join(".tacitus/history/v_x.json"), "{}").unwrap();
        fs::write(dir.join(".tacitus/templates/task.md"), "{{x}}").unwrap();
        fs::write(dir.join(".tacitus/audit.log"), "{}\n").unwrap();
        let mut shadow = ShadowState::default();

        let delta = scan(&dir, &mut shadow).unwrap();
        let mut got = keys(&delta.created);
        got.sort();
        assert_eq!(got, vec!["m:mem_ab12", "n:note"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_ignores_non_markdown_files() {
        let dir = temp_vault("nonmd");
        fs::write(dir.join("image.png"), [0x89u8, 0x50]).unwrap();
        fs::write(dir.join("notes.txt"), "not md").unwrap();
        fs::write(dir.join("real.md"), "md\n").unwrap();
        let mut shadow = ShadowState::default();

        let delta = scan(&dir, &mut shadow).unwrap();
        assert_eq!(keys(&delta.created), vec!["n:real"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unchanged_mtime_and_size_skips_read() {
        let dir = temp_vault("fastpath");
        fs::write(dir.join("a.md"), "stable\n").unwrap();
        let mut shadow = ShadowState::default();
        scan(&dir, &mut shadow).unwrap();

        // Corrupt the shadow hash while keeping (mtime, size) intact: if the
        // fast path skips the read, the lie is never discovered.
        shadow.items.get_mut("n:a").unwrap().hash = "not-the-real-hash".into();
        let delta = scan(&dir, &mut shadow).unwrap();
        assert!(delta.is_empty(), "fast path must not re-hash");
        assert_eq!(shadow.items["n:a"].hash, "not-the-real-hash");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn touched_mtime_same_hash_refreshes_shadow_only() {
        let dir = temp_vault("touch");
        fs::write(dir.join("a.md"), "same content\n").unwrap();
        let mut shadow = ShadowState::default();
        scan(&dir, &mut shadow).unwrap();
        let old_mtime = shadow.items["n:a"].mtime_ms;

        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(dir.join("a.md"), "same content\n").unwrap(); // same bytes, new mtime
        let delta = scan(&dir, &mut shadow).unwrap();
        assert!(delta.is_empty(), "same hash is not a modification");
        assert!(shadow.items["n:a"].mtime_ms >= old_mtime);
        fs::remove_dir_all(&dir).ok();
    }
}
