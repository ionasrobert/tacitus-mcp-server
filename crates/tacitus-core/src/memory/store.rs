use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::types::{Memory, MemoryType, Provenance};

/// Frontmatter view of a Memory (everything except `content`, which is the body).
#[derive(Serialize, Deserialize)]
struct Frontmatter {
    id: String,
    #[serde(rename = "type")]
    memory_type: MemoryType,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    key: Option<String>,
    source: Provenance,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    ttl: Option<u64>,
}

impl Frontmatter {
    fn of(memory: &Memory) -> Self {
        Self {
            id: memory.id.clone(),
            memory_type: memory.memory_type,
            tags: memory.tags.clone(),
            key: memory.key.clone(),
            source: memory.source.clone(),
            ttl: memory.ttl,
        }
    }

    fn into_memory(self, content: String) -> Memory {
        Memory {
            id: self.id,
            memory_type: self.memory_type,
            content,
            tags: self.tags,
            key: self.key,
            source: self.source,
            ttl: self.ttl,
        }
    }
}

/// Persists memories as Markdown + YAML frontmatter under `.tacitus/memory/`
/// — the same on-disk format as the TS engine. Writes are atomic (temp +
/// rename) and idempotent (file named by stable id).
pub struct MemoryStore {
    memory_dir: PathBuf,
}

impl MemoryStore {
    pub fn new(vault_dir: impl AsRef<Path>) -> Self {
        Self {
            memory_dir: vault_dir.as_ref().join(".tacitus").join("memory"),
        }
    }

    pub fn save(&self, memory: &Memory) -> std::io::Result<()> {
        fs::create_dir_all(&self.memory_dir)?;
        let target = self.memory_dir.join(format!("{}.md", memory.id));
        let tmp = self.memory_dir.join(format!(".{}.tmp", memory.id));
        fs::write(&tmp, serialize(memory))?;
        fs::rename(&tmp, &target)?; // same id overwrites → idempotent
        Ok(())
    }

    pub fn load(&self) -> std::io::Result<Vec<Memory>> {
        let entries = match fs::read_dir(&self.memory_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(err) => return Err(err),
        };

        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        files.sort();

        let mut memories = Vec::new();
        for path in files {
            // Corrupt files are skipped, never crash the whole load.
            if let Ok(raw) = fs::read_to_string(&path) {
                if let Some(memory) = parse_memory_file(&raw) {
                    memories.push(memory);
                }
            }
        }
        Ok(memories)
    }

    pub fn remove(&self, id: &str) -> std::io::Result<bool> {
        match fs::remove_file(self.memory_dir.join(format!("{id}.md"))) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }
}

fn serialize(memory: &Memory) -> String {
    let frontmatter =
        serde_yaml::to_string(&Frontmatter::of(memory)).expect("frontmatter serializes");
    format!("---\n{}\n---\n{}\n", frontmatter.trim_end(), memory.content)
}

fn parse_memory_file(raw: &str) -> Option<Memory> {
    let rest = raw.strip_prefix("---\n")?;
    let idx = rest.find("\n---")?;
    let frontmatter = &rest[..idx];
    let after = &rest[idx + 4..];
    let body = after.strip_prefix('\n').unwrap_or(after);
    let content = body.strip_suffix('\n').unwrap_or(body).to_string();
    let parsed: Frontmatter = serde_yaml::from_str(frontmatter).ok()?;
    Some(parsed.into_memory(content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::test_util::{keyed, mem};
    use std::env;

    fn temp_vault() -> PathBuf {
        let mut dir = env::temp_dir();
        // Unique-ish per test via a nanosecond timestamp.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!("tacitus-store-{nanos}"));
        dir
    }

    #[test]
    fn empty_vault_loads_nothing() {
        let store = MemoryStore::new(temp_vault());
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn round_trips_a_memory() {
        let dir = temp_vault();
        let store = MemoryStore::new(&dir);
        let mut memory = keyed("mem_round", "k1", "persisted content\nwith two lines");
        memory.tags = vec!["x".into(), "y".into()];
        store.save(&memory).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, vec![memory]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_idempotent() {
        let dir = temp_vault();
        let store = MemoryStore::new(&dir);
        let memory = mem("mem_dup", "once");
        store.save(&memory).unwrap();
        store.save(&memory).unwrap();
        assert_eq!(store.load().unwrap().len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn skips_corrupt_files() {
        let dir = temp_vault();
        let store = MemoryStore::new(&dir);
        store.save(&mem("mem_good", "valid")).unwrap();
        fs::write(
            dir.join(".tacitus").join("memory").join("corrupt.md"),
            "not frontmatter",
        )
        .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(
            loaded.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["mem_good"]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removes_by_id() {
        let dir = temp_vault();
        let store = MemoryStore::new(&dir);
        store.save(&mem("mem_x", "gone soon")).unwrap();
        assert!(store.remove("mem_x").unwrap());
        assert!(!store.remove("mem_missing").unwrap());
        assert!(store.load().unwrap().is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}
