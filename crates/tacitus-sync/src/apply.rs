//! Materialize merged CRDT state back into the vault — through the engine's
//! transactional `NoteWriter`, so a sync application is versioned,
//! revertible, audited (origin "sync"), and reflected into any live index,
//! exactly like an agent write (Part I.7 observability).
//!
//! Invariant: a local edit is never overwritten. If a file changed on disk
//! since our last scan (a racing editor), we skip it this tick — the next
//! `tick_scan` folds the edit into the CRDT and the merge propagates.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use tacitus_core::vault::{ChangeOp, Changeset, NoteWriter};

use crate::docs::MANIFEST_KEY;
use crate::engine::SyncEngine;
use crate::state::ItemState;
use crate::SyncError;

/// Above this many changed notes (first pull of a big vault) we bypass the
/// history snapshot — full before/after of thousands of notes is bloat, not
/// observability — and record one line in `.tacitus/sync/sync.log` instead.
pub const BULK_BOOTSTRAP_LIMIT: usize = 200;

#[derive(Debug, Default)]
pub struct ApplyReport {
    /// The revertible version id, when the batch went through NoteWriter.
    pub version_id: Option<String>,
    pub notes: Vec<String>,
    pub memories: Vec<String>,
    /// Items skipped because a local edit raced the apply (never overwrite).
    pub skipped: Vec<String>,
}

fn hash_hex(content: &str) -> String {
    use std::fmt::Write;
    let digest = Sha256::digest(content.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn core_err(e: tacitus_core::error::TacitusError) -> SyncError {
    SyncError {
        code: "WRITE",
        reason: format!("{}: {}", e.code, e.reason),
    }
}

impl SyncEngine {
    fn item_path(&self, key: &str) -> Option<(PathBuf, bool)> {
        if let Some(id) = key.strip_prefix("n:") {
            Some((self.vault_dir.join(format!("{id}.md")), true))
        } else {
            key.strip_prefix("m:").map(|id| {
                (
                    self.vault_dir
                        .join(".tacitus")
                        .join("memory")
                        .join(format!("{id}.md")),
                    false,
                )
            })
        }
    }

    /// Write the merged state of `dirty` items into the vault. Notes go
    /// through the writer (one changeset per tick); memories are written
    /// directly (routing them through the writer would pollute the note
    /// index). Call with the `dirty_items` a sync pass accumulated.
    pub fn apply_dirty(
        &mut self,
        writer: &mut NoteWriter,
        dirty: &[String],
    ) -> Result<ApplyReport, SyncError> {
        self.apply_dirty_with_limit(writer, dirty, BULK_BOOTSTRAP_LIMIT)
    }

    pub(crate) fn apply_dirty_with_limit(
        &mut self,
        writer: &mut NoteWriter,
        dirty: &[String],
        bulk_limit: usize,
    ) -> Result<ApplyReport, SyncError> {
        // A manifest update can affect any item (tombstones) — widen to all.
        let mut candidates: BTreeSet<String> = BTreeSet::new();
        for key in dirty {
            if key == MANIFEST_KEY {
                candidates.extend(self.docs.known_items().map_err(SyncError::io)?);
            } else {
                candidates.insert(key.clone());
            }
        }

        let mut report = ApplyReport::default();
        let mut note_ops: Vec<ChangeOp> = Vec::new();
        let mut memory_writes: Vec<(String, PathBuf, Option<String>)> = Vec::new();

        for key in candidates {
            let Some((path, is_note)) = self.item_path(&key) else {
                continue;
            };
            let desired = self.docs.materialize(&key).map_err(SyncError::io)?;
            let on_disk = match fs::read_to_string(&path) {
                Ok(raw) => Some(raw),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(SyncError::io(e)),
            };
            if on_disk == desired {
                continue; // already converged
            }
            // Local-edit race guard: if the disk differs from what our last
            // scan recorded, someone edited since — their edit wins this
            // tick and flows through the next scan.
            let shadow_hash = self.shadow.items.get(&key).map(|s| s.hash.clone());
            let disk_hash = on_disk.as_deref().map(hash_hex);
            if disk_hash != shadow_hash {
                report.skipped.push(key);
                continue;
            }

            if is_note {
                let note_id = key.strip_prefix("n:").unwrap_or(&key).to_string();
                note_ops.push(ChangeOp::SetRaw {
                    note_id,
                    raw: desired.clone(),
                });
                report.notes.push(key.clone());
            } else {
                memory_writes.push((key.clone(), path, desired.clone()));
                report.memories.push(key.clone());
            }
            // Pre-record the post-apply state; fixed up with real stat below.
            self.remember_applied(&key, desired.as_deref());
        }

        // Memories: direct atomic writes, same pattern as MemoryStore::save.
        for (_, path, desired) in &memory_writes {
            match desired {
                Some(raw) => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).map_err(SyncError::io)?;
                    }
                    let tmp = path.with_extension("md.tmp");
                    fs::write(&tmp, raw).map_err(SyncError::io)?;
                    fs::rename(&tmp, path).map_err(SyncError::io)?;
                }
                None => match fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(SyncError::io(e)),
                },
            }
        }

        // Notes: transactional path, or the bulk-bootstrap escape hatch.
        if note_ops.len() > bulk_limit {
            for op in &note_ops {
                let ChangeOp::SetRaw { note_id, raw } = op else {
                    continue;
                };
                let path = self.vault_dir.join(format!("{note_id}.md"));
                match raw {
                    Some(raw) => {
                        if let Some(parent) = path.parent() {
                            fs::create_dir_all(parent).map_err(SyncError::io)?;
                        }
                        let tmp = path.with_extension("md.tmp");
                        fs::write(&tmp, raw).map_err(SyncError::io)?;
                        fs::rename(&tmp, &path).map_err(SyncError::io)?;
                    }
                    None => {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
            let mut log = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.sync_dir.join("sync.log"))
                .map_err(SyncError::io)?;
            writeln!(
                log,
                "{}",
                serde_json::json!({
                    "action": "bulk_apply",
                    "notes": note_ops.len(),
                    "cursor": self.last_seq(),
                })
            )
            .map_err(SyncError::io)?;
        } else if !note_ops.is_empty() {
            let proposal = writer
                .propose(Changeset { ops: note_ops })
                .map_err(core_err)?;
            let commit = writer.commit(&proposal.change_id).map_err(core_err)?;
            report.version_id = Some(commit.version_id);
        }

        // Refresh the shadow with what's actually on disk now, so the next
        // scan doesn't mistake our own apply for a local edit.
        let applied: Vec<String> = report
            .notes
            .iter()
            .chain(report.memories.iter())
            .cloned()
            .collect();
        for key in &applied {
            let Some((path, _)) = self.item_path(key) else {
                continue;
            };
            match fs::metadata(&path) {
                Ok(meta) => {
                    let mtime_ms = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    if let Some(entry) = self.shadow.items.get_mut(key) {
                        entry.mtime_ms = mtime_ms;
                        entry.size = meta.len();
                    }
                }
                Err(_) => {
                    self.shadow.items.remove(key);
                }
            }
        }
        self.shadow.save(&self.sync_dir).map_err(SyncError::io)?;
        Ok(report)
    }

    /// Record the hash we just applied so scan's fast path stays honest.
    fn remember_applied(&mut self, key: &str, desired: Option<&str>) {
        match desired {
            Some(raw) => {
                self.shadow.items.insert(
                    key.to_string(),
                    ItemState {
                        hash: hash_hex(raw),
                        mtime_ms: 0, // fixed up after the write with real stat
                        size: raw.len() as u64,
                    },
                );
            }
            None => {
                self.shadow.items.remove(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::VaultCode;
    use crate::protocol::{ClientMsg, ServerMsg};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tacitus_core::vault::PermissionScope;

    fn temp_vault(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-apply-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Push A's changes straight into B (in-memory relay with seq counter).
    fn pump(from: &mut SyncEngine, to: &mut SyncEngine, next_seq: &mut u64) -> Vec<String> {
        let mut dirty = Vec::new();
        for msg in from.tick_scan().unwrap() {
            let ClientMsg::Push { blob } = msg else {
                continue;
            };
            let effect = to
                .on_server_msg(ServerMsg::Update {
                    seq: *next_seq,
                    blob,
                })
                .unwrap();
            dirty.extend(effect.dirty_items);
            *next_seq += 1;
        }
        dirty
    }

    #[test]
    fn remote_batch_applies_via_notewriter_with_version_id() {
        let da = temp_vault("nw-a");
        let db = temp_vault("nw-b");
        fs::write(da.join("shared.md"), "# Shared\n\nfrom A\n").unwrap();
        let code = VaultCode::generate();
        let mut a = SyncEngine::open(&da, &code).unwrap();
        let mut b = SyncEngine::open(&db, &code).unwrap();

        let mut seq = 1;
        let dirty = pump(&mut a, &mut b, &mut seq);
        let mut writer = NoteWriter::new(&db, PermissionScope::ReadWrite);
        writer.set_origin("sync");
        let report = b.apply_dirty(&mut writer, &dirty).unwrap();

        // The file exists on B's disk, byte-identical.
        assert_eq!(
            fs::read_to_string(db.join("shared.md")).unwrap(),
            "# Shared\n\nfrom A\n"
        );
        let version_id = report.version_id.expect("went through NoteWriter");

        // …audited with origin "sync"…
        let audit = writer.read_audit(1).unwrap();
        assert_eq!(audit[0].origin.as_deref(), Some("sync"));
        assert_eq!(audit[0].version_id, version_id);

        // …and revertible like any agent write.
        writer.revert(&version_id).unwrap();
        assert!(!db.join("shared.md").exists());
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn local_edit_during_apply_is_never_overwritten() {
        let da = temp_vault("race-a");
        let db = temp_vault("race-b");
        fs::write(da.join("doc.md"), "remote version\n").unwrap();
        fs::write(db.join("doc.md"), "local baseline\n").unwrap();
        let code = VaultCode::generate();
        let mut a = SyncEngine::open(&da, &code).unwrap();
        let mut b = SyncEngine::open(&db, &code).unwrap();

        // B scans its baseline, then the user edits AFTER the scan.
        b.tick_scan().unwrap();
        fs::write(db.join("doc.md"), "local edit AFTER the scan\n").unwrap();

        let mut seq = 1;
        let dirty = pump(&mut a, &mut b, &mut seq);
        let mut writer = NoteWriter::new(&db, PermissionScope::ReadWrite);
        let report = b.apply_dirty(&mut writer, &dirty).unwrap();

        assert!(report.skipped.contains(&"n:doc".to_string()));
        assert_eq!(
            fs::read_to_string(db.join("doc.md")).unwrap(),
            "local edit AFTER the scan\n",
            "the racing local edit survives untouched"
        );
        // Next tick folds the local edit into the CRDT — nothing lost.
        let pushes = b.tick_scan().unwrap();
        assert!(!pushes.is_empty());
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn bulk_bootstrap_bypasses_history_and_logs_once() {
        let da = temp_vault("bulk-a");
        let db = temp_vault("bulk-b");
        for i in 0..3 {
            fs::write(da.join(format!("note{i}.md")), format!("content {i}\n")).unwrap();
        }
        let code = VaultCode::generate();
        let mut a = SyncEngine::open(&da, &code).unwrap();
        let mut b = SyncEngine::open(&db, &code).unwrap();

        let mut seq = 1;
        let dirty = pump(&mut a, &mut b, &mut seq);
        let mut writer = NoteWriter::new(&db, PermissionScope::ReadWrite);
        // bulk_limit 2 < 3 notes → the escape hatch fires.
        let report = b.apply_dirty_with_limit(&mut writer, &dirty, 2).unwrap();

        assert_eq!(report.version_id, None, "no history snapshot for bulk");
        for i in 0..3 {
            assert!(db.join(format!("note{i}.md")).exists());
        }
        assert!(!db.join(".tacitus").join("history").exists());
        let sync_log =
            fs::read_to_string(db.join(".tacitus").join("sync").join("sync.log")).unwrap();
        assert_eq!(sync_log.lines().count(), 1);
        assert!(sync_log.contains("bulk_apply"));
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn memories_apply_directly_to_memory_dir() {
        let da = temp_vault("mem-a");
        let db = temp_vault("mem-b");
        fs::create_dir_all(da.join(".tacitus/memory")).unwrap();
        fs::write(
            da.join(".tacitus/memory/mem_x1.md"),
            "---\nid: mem_x1\n---\nAgent memory travels.\n",
        )
        .unwrap();
        let code = VaultCode::generate();
        let mut a = SyncEngine::open(&da, &code).unwrap();
        let mut b = SyncEngine::open(&db, &code).unwrap();

        let mut seq = 1;
        let dirty = pump(&mut a, &mut b, &mut seq);
        let mut writer = NoteWriter::new(&db, PermissionScope::ReadWrite);
        let report = b.apply_dirty(&mut writer, &dirty).unwrap();

        assert_eq!(report.memories, vec!["m:mem_x1"]);
        assert_eq!(report.version_id, None, "memories skip the note history");
        assert!(db.join(".tacitus/memory/mem_x1.md").exists());
        // Applying our own write is not re-detected as a local change.
        assert!(b.tick_scan().unwrap().is_empty());
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn applied_notes_do_not_echo_back_as_local_edits() {
        let da = temp_vault("echo-a");
        let db = temp_vault("echo-b");
        fs::write(da.join("note.md"), "hello\n").unwrap();
        let code = VaultCode::generate();
        let mut a = SyncEngine::open(&da, &code).unwrap();
        let mut b = SyncEngine::open(&db, &code).unwrap();

        let mut seq = 1;
        let dirty = pump(&mut a, &mut b, &mut seq);
        let mut writer = NoteWriter::new(&db, PermissionScope::ReadWrite);
        b.apply_dirty(&mut writer, &dirty).unwrap();

        assert!(
            b.tick_scan().unwrap().is_empty(),
            "our own apply must not look like a local edit"
        );
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }
}
