use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use super::parse::parse_note;
use super::types::Note;

/// In-memory snapshot of a vault's notes plus its wikilink graph.
pub struct VaultIndex {
    notes: HashMap<String, Note>,
}

impl VaultIndex {
    pub fn build(vault_dir: &Path) -> std::io::Result<Self> {
        let mut notes = HashMap::new();
        let mut stack = vec![vault_dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().and_then(|n| n.to_str()) != Some(".tacitus") {
                        stack.push(path);
                    }
                } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    if let Ok(raw) = fs::read_to_string(&path) {
                        let rel = path.strip_prefix(vault_dir).unwrap_or(&path);
                        let note = parse_note(&raw, &rel.to_string_lossy().replace('\\', "/"));
                        notes.insert(note.id.clone(), note);
                    }
                }
            }
        }
        Ok(Self { notes })
    }

    pub fn all(&self) -> Vec<&Note> {
        self.notes.values().collect()
    }

    pub fn get(&self, id: &str) -> Option<&Note> {
        self.notes.get(id)
    }

    /// Reflect a written note into the live index (used after a commit/revert).
    pub fn upsert_raw(&mut self, rel_path: &str, raw: &str) {
        let note = parse_note(raw, rel_path);
        self.notes.insert(note.id.clone(), note);
    }

    pub fn remove_note(&mut self, id: &str) {
        self.notes.remove(id);
    }

    /// Resolve a wikilink target: exact id first, then basename (case-insensitive).
    pub fn resolve(&self, target: &str) -> Option<&Note> {
        if let Some(note) = self.notes.get(target) {
            return Some(note);
        }
        let wanted = target.to_lowercase();
        self.notes
            .values()
            .find(|n| n.id.rsplit('/').next().unwrap_or(&n.id).to_lowercase() == wanted)
    }

    pub fn outgoing(&self, id: &str) -> Vec<&Note> {
        let Some(note) = self.notes.get(id) else {
            return vec![];
        };
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for link in &note.links {
            if let Some(target) = self.resolve(&link.target) {
                if target.id != id && seen.insert(target.id.clone()) {
                    out.push(target);
                }
            }
        }
        out
    }

    pub fn backlinks(&self, id: &str) -> Vec<&Note> {
        if !self.notes.contains_key(id) {
            return vec![];
        }
        self.notes
            .values()
            .filter(|n| {
                n.id != id
                    && n.links
                        .iter()
                        .any(|l| self.resolve(&l.target).map(|t| t.id.as_str()) == Some(id))
            })
            .collect()
    }
}
