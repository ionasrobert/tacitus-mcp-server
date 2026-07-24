//! Transactional write-back for vault notes (Part I.4), the Rust port of the TS
//! `NoteWriter` (`packages/mcp-server/src/vault/write.ts`).
//!
//! A changeset is validated and previewed with [`NoteWriter::propose`] (no disk
//! mutation), applied atomically with [`NoteWriter::commit`] (all-or-nothing,
//! rolling back on any failure and writing a reversible version snapshot), and
//! undone with [`NoteWriter::revert`]. Read-only scope forbids mutation. The
//! convenience helpers (`create_note`/`update_note`/`link`/`tag`) auto-commit but
//! stay transactional, versioned, and audited. Every mutation is appended to a
//! JSONL audit log.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::error::TacitusError;
use crate::ids::stable_id;

use super::index::VaultIndex;
use super::parse::parse_note;

/// Permission scope for a session. Default is minimum-privilege friendly: a
/// read-only scope can dry-run (`propose`) but never mutate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionScope {
    ReadOnly,
    ReadWrite,
}

/// One operation in a changeset.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum ChangeOp {
    Create {
        note_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        frontmatter: Option<Value>,
    },
    Update {
        note_id: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        frontmatter: Option<Value>,
    },
    Delete {
        note_id: String,
    },
    /// Set a note's raw file contents verbatim (None = delete). Byte-faithful
    /// and tolerant: no create/update distinction, no conflict on existence —
    /// this is the op the sync layer applies merged CRDT state with, where
    /// last-merged-wins is the semantic and byte fidelity is the invariant.
    SetRaw {
        note_id: String,
        raw: Option<String>,
    },
}

impl ChangeOp {
    fn note_id(&self) -> &str {
        match self {
            ChangeOp::Create { note_id, .. }
            | ChangeOp::Update { note_id, .. }
            | ChangeOp::Delete { note_id }
            | ChangeOp::SetRaw { note_id, .. } => note_id,
        }
    }
}

/// A batch of operations applied atomically.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Changeset {
    pub ops: Vec<ChangeOp>,
}

/// A single before/after entry in a proposal's diff.
#[derive(Clone, Debug, Serialize)]
pub struct DiffEntry {
    pub note_id: String,
    pub op: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Result of a dry-run: a stable `change_id` plus the before/after diff.
#[derive(Clone, Debug, Serialize)]
pub struct Proposal {
    pub change_id: String,
    pub diff: Vec<DiffEntry>,
}

/// Result of a successful commit: a reversible `version_id`.
#[derive(Clone, Debug, Serialize)]
pub struct CommitResult {
    pub version_id: String,
}

/// Result of a revert.
#[derive(Clone, Debug, Serialize)]
pub struct RevertResult {
    pub reverted: bool,
    pub version_id: String,
}

/// On-disk snapshot written per commit under `.tacitus/history/` — enough to
/// undo the commit by restoring every affected note's prior raw contents.
#[derive(Serialize, Deserialize)]
struct Snapshot {
    version_id: String,
    change_id: String,
    /// note_id → prior raw file contents, or null if the note did not exist.
    before: BTreeMap<String, Option<String>>,
    /// note_id → new raw file contents, or null if the note was deleted.
    after: BTreeMap<String, Option<String>>,
}

/// One appended line in the agent-action audit log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: String,
    pub action: String,
    pub version_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub change_id: Option<String>,
    pub notes: Vec<String>,
    pub scope: PermissionScope,
    /// Who initiated the write when it wasn't a direct agent/tool call —
    /// e.g. "sync". Absent for ordinary commits (backward compatible).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub origin: Option<String>,
}

/// One note's change inside a committed version (from the history snapshot).
#[derive(Clone, Debug, Serialize)]
pub struct VersionNoteChange {
    pub note_id: String,
    /// "created" | "updated" | "deleted"
    pub op: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// A committed version, fully expanded — what changed, note by note.
#[derive(Clone, Debug, Serialize)]
pub struct VersionDetail {
    pub version_id: String,
    pub change_id: String,
    pub notes: Vec<VersionNoteChange>,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Serialize a note file the same way the TS engine does: frontmatter block only
/// when non-empty, body followed by a trailing newline.
fn serialize(frontmatter: Option<&Value>, content: &str) -> String {
    let non_empty = frontmatter.is_some_and(|v| match v {
        Value::Mapping(m) => !m.is_empty(),
        Value::Null => false,
        _ => true,
    });
    if non_empty {
        let yaml = serde_yaml::to_string(frontmatter.unwrap()).unwrap_or_default();
        format!("---\n{}\n---\n{content}\n", yaml.trim_end())
    } else {
        format!("{content}\n")
    }
}

fn read_raw_or_null(path: &Path) -> std::io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Frontmatter tags as declared (a sequence of strings), mirroring the TS
/// `Array.isArray(frontmatter.tags)` check — inline `#tags` are not included.
fn frontmatter_tags(fm: &Value) -> Vec<String> {
    match fm.get("tags") {
        Some(Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => vec![],
    }
}

fn with_tags(fm: &Value, tags: Vec<String>) -> Value {
    let mut map = match fm {
        Value::Mapping(m) => m.clone(),
        _ => serde_yaml::Mapping::new(),
    };
    map.insert(
        Value::String("tags".into()),
        Value::Sequence(tags.into_iter().map(Value::String).collect()),
    );
    Value::Mapping(map)
}

pub struct NoteWriter {
    vault_dir: PathBuf,
    tacitus_dir: PathBuf,
    history_dir: PathBuf,
    audit_path: PathBuf,
    scope: PermissionScope,
    index: Option<Arc<Mutex<VaultIndex>>>,
    pending: HashMap<String, Changeset>,
    origin: Option<String>,
}

impl NoteWriter {
    pub fn new(vault_dir: impl AsRef<Path>, scope: PermissionScope) -> Self {
        Self::build(vault_dir, scope, None)
    }

    /// Like [`NoteWriter::new`] but reflects committed/reverted state into a
    /// shared live index (used after a commit/revert).
    pub fn with_index(
        vault_dir: impl AsRef<Path>,
        scope: PermissionScope,
        index: Arc<Mutex<VaultIndex>>,
    ) -> Self {
        Self::build(vault_dir, scope, Some(index))
    }

    fn build(
        vault_dir: impl AsRef<Path>,
        scope: PermissionScope,
        index: Option<Arc<Mutex<VaultIndex>>>,
    ) -> Self {
        let vault_dir = vault_dir.as_ref().to_path_buf();
        let tacitus_dir = vault_dir.join(".tacitus");
        let history_dir = tacitus_dir.join("history");
        let audit_path = tacitus_dir.join("audit.log");
        Self {
            vault_dir,
            tacitus_dir,
            history_dir,
            audit_path,
            scope,
            index,
            pending: HashMap::new(),
            origin: None,
        }
    }

    /// Mark every subsequent commit/revert in the audit log with an origin
    /// (e.g. "sync"), distinguishing it from direct agent writes.
    pub fn set_origin(&mut self, origin: impl Into<String>) {
        self.origin = Some(origin.into());
    }

    fn note_path(&self, note_id: &str) -> PathBuf {
        self.vault_dir.join(format!("{note_id}.md"))
    }

    // ---- Convenience helpers (auto-commit; still transactional + audited) ----

    pub fn create_note(
        &mut self,
        note_id: &str,
        content: &str,
        frontmatter: Option<Value>,
    ) -> Result<CommitResult, TacitusError> {
        self.apply(Changeset {
            ops: vec![ChangeOp::Create {
                note_id: note_id.to_string(),
                content: content.to_string(),
                frontmatter,
            }],
        })
    }

    pub fn update_note(
        &mut self,
        note_id: &str,
        content: Option<String>,
        frontmatter: Option<Value>,
    ) -> Result<CommitResult, TacitusError> {
        self.apply(Changeset {
            ops: vec![ChangeOp::Update {
                note_id: note_id.to_string(),
                content,
                frontmatter,
            }],
        })
    }

    /// Delete a note (auto-committed) — versioned and revertible.
    pub fn delete_note(&mut self, note_id: &str) -> Result<CommitResult, TacitusError> {
        self.apply(Changeset {
            ops: vec![ChangeOp::Delete {
                note_id: note_id.to_string(),
            }],
        })
    }

    /// Append a `[[to]]` wikilink to the source note (idempotent).
    pub fn link(&mut self, from: &str, to: &str) -> Result<CommitResult, TacitusError> {
        let note = self.current_note(from)?.ok_or_else(|| not_found(from))?;
        let marker = format!("[[{to}]]");
        let content = if note.content.contains(&marker) {
            note.content
        } else {
            format!("{}\n\n{marker}\n", note.content.trim_end())
        };
        self.apply(Changeset {
            ops: vec![ChangeOp::Update {
                note_id: from.to_string(),
                content: Some(content),
                frontmatter: None,
            }],
        })
    }

    /// Add a tag to the note's frontmatter (deduplicated).
    pub fn tag(&mut self, note_id: &str, tag: &str) -> Result<CommitResult, TacitusError> {
        let note = self
            .current_note(note_id)?
            .ok_or_else(|| not_found(note_id))?;
        let mut tags = frontmatter_tags(&note.frontmatter);
        if !tags.iter().any(|t| t == tag) {
            tags.push(tag.to_string());
        }
        let frontmatter = with_tags(&note.frontmatter, tags);
        self.apply(Changeset {
            ops: vec![ChangeOp::Update {
                note_id: note_id.to_string(),
                content: None,
                frontmatter: Some(frontmatter),
            }],
        })
    }

    fn apply(&mut self, changeset: Changeset) -> Result<CommitResult, TacitusError> {
        let Proposal { change_id, .. } = self.propose(changeset)?;
        self.commit(&change_id)
    }

    fn current_note(&self, note_id: &str) -> Result<Option<super::types::Note>, TacitusError> {
        match read_raw_or_null(&self.note_path(note_id))? {
            Some(raw) => Ok(Some(parse_note(&raw, &format!("{note_id}.md")))),
            None => Ok(None),
        }
    }

    /// Dry-run: validate the changeset and return a before/after diff. No writes.
    pub fn propose(&mut self, changeset: Changeset) -> Result<Proposal, TacitusError> {
        let diff = self.build_diff(&changeset)?;
        let seed = serde_json::to_string(&changeset).map_err(serde_internal)?;
        let change_id = stable_id(&seed, "chg");
        self.pending.insert(change_id.clone(), changeset);
        Ok(Proposal { change_id, diff })
    }

    pub fn commit(&mut self, change_id: &str) -> Result<CommitResult, TacitusError> {
        self.assert_writable()?;
        let changeset = self.pending.get(change_id).cloned().ok_or_else(|| {
            TacitusError::new(
                "UNKNOWN_CHANGE",
                format!("No pending change with id \"{change_id}\"."),
                "Call propose_changes first and use the returned change_id.",
            )
        })?;

        // Re-validate against current disk state (may have changed since propose).
        let diff = self.build_diff(&changeset)?;

        let mut before: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut after: BTreeMap<String, Option<String>> = BTreeMap::new();
        for entry in &diff {
            before.insert(entry.note_id.clone(), entry.before.clone());
            after.insert(entry.note_id.clone(), entry.after.clone());
        }

        // Apply atomically: on any failure, roll back everything from `before`.
        let mut done: Vec<String> = Vec::new();
        for entry in &diff {
            if let Err(err) = self.apply_state(&entry.note_id, entry.after.as_deref()) {
                for note_id in &done {
                    let _ =
                        self.apply_state(note_id, before.get(note_id).and_then(|o| o.as_deref()));
                }
                return Err(err.into());
            }
            done.push(entry.note_id.clone());
        }

        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let version_id = stable_id(&format!("{change_id}:{millis}"), "v");
        self.write_snapshot(&Snapshot {
            version_id: version_id.clone(),
            change_id: change_id.to_string(),
            before,
            after: after.clone(),
        })?;
        self.reflect(&after);
        self.append_audit(&AuditEntry {
            ts: now_iso(),
            action: "commit".into(),
            version_id: version_id.clone(),
            change_id: Some(change_id.to_string()),
            notes: after.keys().cloned().collect(),
            scope: self.scope,
            origin: self.origin.clone(),
        })?;
        self.pending.remove(change_id);
        Ok(CommitResult { version_id })
    }

    pub fn revert(&self, version_id: &str) -> Result<RevertResult, TacitusError> {
        self.assert_writable()?;
        let raw = read_raw_or_null(&self.history_dir.join(format!("{version_id}.json")))?
            .ok_or_else(|| {
                TacitusError::new(
                    "UNKNOWN_VERSION",
                    format!("No version with id \"{version_id}\"."),
                    "Use a version_id returned by commit_changes.",
                )
            })?;
        let snapshot: Snapshot = serde_json::from_str(&raw).map_err(|e| {
            TacitusError::new(
                "INTERNAL",
                e.to_string(),
                "The history snapshot is corrupt.",
            )
        })?;
        for (note_id, prior_raw) in &snapshot.before {
            self.apply_state(note_id, prior_raw.as_deref())?;
        }
        self.reflect(&snapshot.before);
        self.append_audit(&AuditEntry {
            ts: now_iso(),
            action: "revert".into(),
            version_id: version_id.to_string(),
            change_id: None,
            notes: snapshot.before.keys().cloned().collect(),
            scope: self.scope,
            origin: self.origin.clone(),
        })?;
        Ok(RevertResult {
            reverted: true,
            version_id: version_id.to_string(),
        })
    }

    /// Read the agent-action audit log, most recent first.
    pub fn read_audit(&self, limit: usize) -> Result<Vec<AuditEntry>, TacitusError> {
        let Some(raw) = read_raw_or_null(&self.audit_path)? else {
            return Ok(vec![]);
        };
        let mut entries: Vec<AuditEntry> = raw
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        entries.reverse();
        entries.truncate(limit);
        Ok(entries)
    }

    /// Expand a committed version's snapshot: per-note before/after (Part I.7
    /// observability — see WHAT an agent changed, not just that it did).
    pub fn get_version(&self, version_id: &str) -> Result<VersionDetail, TacitusError> {
        let raw = read_raw_or_null(&self.history_dir.join(format!("{version_id}.json")))?
            .ok_or_else(|| {
                TacitusError::new(
                    "UNKNOWN_VERSION",
                    format!("No version with id \"{version_id}\"."),
                    "Use a version_id from audit_log.",
                )
            })?;
        let snapshot: Snapshot = serde_json::from_str(&raw).map_err(|e| {
            TacitusError::new(
                "INTERNAL",
                e.to_string(),
                "The history snapshot is corrupt.",
            )
        })?;
        let notes = snapshot
            .before
            .iter()
            .map(|(note_id, before)| {
                let after = snapshot.after.get(note_id).cloned().flatten();
                let op = match (before.is_some(), after.is_some()) {
                    (false, true) => "created",
                    (true, false) => "deleted",
                    _ => "updated",
                };
                VersionNoteChange {
                    note_id: note_id.clone(),
                    op: op.to_string(),
                    before: before.clone(),
                    after,
                }
            })
            .collect();
        Ok(VersionDetail {
            version_id: snapshot.version_id,
            change_id: snapshot.change_id,
            notes,
        })
    }

    fn append_audit(&self, entry: &AuditEntry) -> Result<(), TacitusError> {
        fs::create_dir_all(&self.tacitus_dir)?;
        let line = serde_json::to_string(entry).map_err(serde_internal)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_path)?;
        file.write_all(format!("{line}\n").as_bytes())?;
        Ok(())
    }

    fn assert_writable(&self) -> Result<(), TacitusError> {
        if self.scope == PermissionScope::ReadOnly {
            return Err(TacitusError::new(
                "PERMISSION_DENIED",
                "This session is read-only; writes are not permitted.",
                "Re-open the vault with a read-write scope to apply changes.",
            ));
        }
        Ok(())
    }

    fn build_diff(&self, changeset: &Changeset) -> Result<Vec<DiffEntry>, TacitusError> {
        let mut diff = Vec::with_capacity(changeset.ops.len());
        for op in &changeset.ops {
            let before = read_raw_or_null(&self.note_path(op.note_id()))?;
            match op {
                ChangeOp::Create {
                    note_id,
                    content,
                    frontmatter,
                } => {
                    if before.is_some() {
                        return Err(TacitusError::new(
                            "CONFLICT",
                            format!("Note \"{note_id}\" already exists."),
                            "Use an update op, or choose a different note_id.",
                        ));
                    }
                    diff.push(DiffEntry {
                        note_id: note_id.clone(),
                        op: "create".into(),
                        before: None,
                        after: Some(serialize(frontmatter.as_ref(), content)),
                    });
                }
                ChangeOp::Update {
                    note_id,
                    content,
                    frontmatter,
                } => {
                    let Some(before_raw) = before else {
                        return Err(not_found(note_id));
                    };
                    let existing = parse_note(&before_raw, &format!("{note_id}.md"));
                    let fm = frontmatter.clone().unwrap_or(existing.frontmatter);
                    let body = content.clone().unwrap_or(existing.content);
                    diff.push(DiffEntry {
                        note_id: note_id.clone(),
                        op: "update".into(),
                        before: Some(before_raw),
                        after: Some(serialize(Some(&fm), &body)),
                    });
                }
                ChangeOp::SetRaw { note_id, raw } => {
                    let op = match (before.is_some(), raw.is_some()) {
                        (false, true) => "create",
                        (true, false) => "delete",
                        _ => "update",
                    };
                    diff.push(DiffEntry {
                        note_id: note_id.clone(),
                        op: op.into(),
                        before,
                        after: raw.clone(),
                    });
                }
                ChangeOp::Delete { note_id } => {
                    let Some(before_raw) = before else {
                        return Err(not_found(note_id));
                    };
                    diff.push(DiffEntry {
                        note_id: note_id.clone(),
                        op: "delete".into(),
                        before: Some(before_raw),
                        after: None,
                    });
                }
            }
        }
        Ok(diff)
    }

    /// Bring a note file to the given state (`None` = deleted) with an atomic write.
    fn apply_state(&self, note_id: &str, raw: Option<&str>) -> std::io::Result<()> {
        let path = self.note_path(note_id);
        match raw {
            None => match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err),
            },
            Some(content) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let tmp = path.with_extension("md.tmp");
                fs::write(&tmp, content)?;
                fs::rename(&tmp, &path)?;
                Ok(())
            }
        }
    }

    fn reflect(&self, state: &BTreeMap<String, Option<String>>) {
        let Some(index) = &self.index else {
            return;
        };
        let mut index = index.lock().expect("index mutex poisoned");
        for (note_id, raw) in state {
            match raw {
                None => index.remove_note(note_id),
                Some(raw) => index.upsert_raw(&format!("{note_id}.md"), raw),
            }
        }
    }

    fn write_snapshot(&self, snapshot: &Snapshot) -> Result<(), TacitusError> {
        fs::create_dir_all(&self.history_dir)?;
        let path = self
            .history_dir
            .join(format!("{}.json", snapshot.version_id));
        let tmp = self
            .history_dir
            .join(format!(".{}.json.tmp", snapshot.version_id));
        fs::write(
            &tmp,
            serde_json::to_string_pretty(snapshot).map_err(serde_internal)?,
        )?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn not_found(note_id: &str) -> TacitusError {
    TacitusError::new(
        "NOTE_NOT_FOUND",
        format!("No note with id \"{note_id}\"."),
        "Create it first, or check the id with list_notes.",
    )
}

fn serde_internal(err: serde_json::Error) -> TacitusError {
    TacitusError::new(
        "INTERNAL",
        err.to_string(),
        "This is a bug; please report it.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-write-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn create_op(note_id: &str, content: &str) -> Changeset {
        Changeset {
            ops: vec![ChangeOp::Create {
                note_id: note_id.into(),
                content: content.into(),
                frontmatter: None,
            }],
        }
    }

    // ---- M7: transactional write-back (mirrors write.test.ts) ----

    #[test]
    fn propose_is_a_dry_run_touching_no_files() {
        let dir = temp_vault("dryrun");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        let proposal = writer.propose(create_op("notes/new", "hello")).unwrap();
        assert!(proposal.change_id.starts_with("chg_"));
        assert_eq!(proposal.diff.len(), 1);
        assert_eq!(proposal.diff[0].note_id, "notes/new");
        assert_eq!(proposal.diff[0].op, "create");
        assert!(proposal.diff[0].before.is_none());
        assert!(proposal.diff[0].after.as_deref().unwrap().contains("hello"));
        assert!(!dir.join("notes").join("new.md").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_applies_the_changeset_to_disk() {
        let dir = temp_vault("commit");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        let mut fm = serde_yaml::Mapping::new();
        fm.insert(Value::String("title".into()), Value::String("A".into()));
        let proposal = writer
            .propose(Changeset {
                ops: vec![ChangeOp::Create {
                    note_id: "a".into(),
                    content: "body".into(),
                    frontmatter: Some(Value::Mapping(fm)),
                }],
            })
            .unwrap();
        let result = writer.commit(&proposal.change_id).unwrap();
        assert!(result.version_id.starts_with("v_"));
        let raw = fs::read_to_string(dir.join("a.md")).unwrap();
        assert!(raw.contains("title: A"));
        assert!(raw.contains("body"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_creating_over_an_existing_note_at_propose() {
        let dir = temp_vault("conflict");
        fs::write(dir.join("a.md"), "existing").unwrap();
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        let err = writer.propose(create_op("a", "x")).unwrap_err();
        assert_eq!(err.code, "CONFLICT");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_a_missing_note_update_and_stays_all_or_nothing() {
        let dir = temp_vault("allornothing");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        let err = writer
            .propose(Changeset {
                ops: vec![
                    ChangeOp::Create {
                        note_id: "ok".into(),
                        content: "x".into(),
                        frontmatter: None,
                    },
                    ChangeOp::Update {
                        note_id: "missing".into(),
                        content: Some("y".into()),
                        frontmatter: None,
                    },
                ],
            })
            .unwrap_err();
        assert_eq!(err.code, "NOTE_NOT_FOUND");
        assert!(!dir.join("ok.md").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn revert_removes_a_note_that_a_create_introduced() {
        let dir = temp_vault("revertcreate");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        let proposal = writer.propose(create_op("temp", "x")).unwrap();
        let commit = writer.commit(&proposal.change_id).unwrap();
        assert!(dir.join("temp.md").exists());
        writer.revert(&commit.version_id).unwrap();
        assert!(!dir.join("temp.md").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn revert_restores_prior_content_after_an_update() {
        let dir = temp_vault("revertupdate");
        fs::write(dir.join("doc.md"), "OLD").unwrap();
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        let proposal = writer
            .propose(Changeset {
                ops: vec![ChangeOp::Update {
                    note_id: "doc".into(),
                    content: Some("NEW".into()),
                    frontmatter: None,
                }],
            })
            .unwrap();
        let commit = writer.commit(&proposal.change_id).unwrap();
        assert!(fs::read_to_string(dir.join("doc.md"))
            .unwrap()
            .contains("NEW"));
        writer.revert(&commit.version_id).unwrap();
        assert_eq!(fs::read_to_string(dir.join("doc.md")).unwrap(), "OLD");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn propose_is_idempotent_same_changeset_same_change_id() {
        let dir = temp_vault("idempotent");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        let a = writer.propose(create_op("z", "zz")).unwrap().change_id;
        let b = writer.propose(create_op("z", "zz")).unwrap().change_id;
        assert_eq!(a, b);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_only_scope_denies_commit_but_allows_propose() {
        let dir = temp_vault("readonly");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadOnly);
        let proposal = writer.propose(create_op("x", "x")).unwrap();
        let err = writer.commit(&proposal.change_id).unwrap_err();
        assert_eq!(err.code, "PERMISSION_DENIED");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reflects_committed_changes_into_a_provided_live_index() {
        let dir = temp_vault("reflect");
        let index = Arc::new(Mutex::new(VaultIndex::build(&dir).unwrap()));
        let mut writer = NoteWriter::with_index(&dir, PermissionScope::ReadWrite, index.clone());
        let proposal = writer.propose(create_op("live", "live note")).unwrap();
        writer.commit(&proposal.change_id).unwrap();
        assert_eq!(
            index.lock().unwrap().get("live").map(|n| n.title.clone()),
            Some("live".to_string())
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn get_version_classifies_created_updated_deleted() {
        let dir = temp_vault("getversion");
        fs::write(dir.join("edit.md"), "OLD\n").unwrap();
        fs::write(dir.join("gone.md"), "BYE\n").unwrap();
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        let proposal = writer
            .propose(Changeset {
                ops: vec![
                    ChangeOp::Create {
                        note_id: "fresh".into(),
                        content: "NEW".into(),
                        frontmatter: None,
                    },
                    ChangeOp::Update {
                        note_id: "edit".into(),
                        content: Some("CHANGED".into()),
                        frontmatter: None,
                    },
                    ChangeOp::Delete {
                        note_id: "gone".into(),
                    },
                ],
            })
            .unwrap();
        let version = writer.commit(&proposal.change_id).unwrap().version_id;

        let detail = writer.get_version(&version).unwrap();
        assert_eq!(detail.version_id, version);
        let op_of = |id: &str| {
            detail
                .notes
                .iter()
                .find(|n| n.note_id == id)
                .unwrap()
                .op
                .clone()
        };
        assert_eq!(op_of("fresh"), "created");
        assert_eq!(op_of("edit"), "updated");
        assert_eq!(op_of("gone"), "deleted");
        let edit = detail.notes.iter().find(|n| n.note_id == "edit").unwrap();
        assert!(edit.before.as_deref().unwrap().contains("OLD"));
        assert!(edit.after.as_deref().unwrap().contains("CHANGED"));

        assert_eq!(
            writer.get_version("v_nope").unwrap_err().code,
            "UNKNOWN_VERSION"
        );
        fs::remove_dir_all(&dir).ok();
    }

    // ---- M9: convenience helpers + audit log (mirrors convenience.test.ts) ----

    #[test]
    fn create_note_writes_and_returns_version_id() {
        let dir = temp_vault("createnote");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        let mut fm = serde_yaml::Mapping::new();
        fm.insert(Value::String("title".into()), Value::String("A".into()));
        let result = writer
            .create_note("notes/a", "hello", Some(Value::Mapping(fm)))
            .unwrap();
        assert!(result.version_id.starts_with("v_"));
        assert!(fs::read_to_string(dir.join("notes").join("a.md"))
            .unwrap()
            .contains("hello"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_note_replaces_content() {
        let dir = temp_vault("updatenote");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        writer.create_note("a", "old", None).unwrap();
        writer.update_note("a", Some("new".into()), None).unwrap();
        assert!(fs::read_to_string(dir.join("a.md"))
            .unwrap()
            .contains("new"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn link_appends_a_wikilink_to_the_source_note() {
        let dir = temp_vault("link");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        writer.create_note("a", "body", None).unwrap();
        writer.create_note("b", "other", None).unwrap();
        writer.link("a", "b").unwrap();
        let note = parse_note(&fs::read_to_string(dir.join("a.md")).unwrap(), "a.md");
        assert!(note.links.iter().any(|l| l.target == "b"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tag_adds_a_frontmatter_tag_and_dedupes() {
        let dir = temp_vault("tag");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        writer.create_note("a", "body", None).unwrap();
        writer.tag("a", "urgent").unwrap();
        writer.tag("a", "urgent").unwrap();
        let note = parse_note(&fs::read_to_string(dir.join("a.md")).unwrap(), "a.md");
        assert!(note.tags.contains(&"urgent".to_string()));
        assert_eq!(frontmatter_tags(&note.frontmatter), vec!["urgent"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn records_an_audit_entry_per_mutation_most_recent_first() {
        let dir = temp_vault("audit");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        writer.create_note("a", "x", None).unwrap();
        writer.create_note("b", "y", None).unwrap();
        let audit = writer.read_audit(50).unwrap();
        assert!(audit.len() >= 2);
        assert_eq!(audit[0].action, "commit");
        assert!(audit[0].notes.contains(&"b".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_only_scope_denies_create_note() {
        let dir = temp_vault("readonlycreate");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadOnly);
        let err = writer.create_note("a", "x", None).unwrap_err();
        assert_eq!(err.code, "PERMISSION_DENIED");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn setraw_writes_bytes_verbatim_and_is_versioned() {
        let dir = temp_vault("setraw");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        // Raw content with frontmatter, odd spacing, and a single trailing
        // newline — must land on disk byte-for-byte, no reserialization.
        let raw = "---\ntitle: Exact   # comment kept\n---\nbody line\n";
        let proposal = writer
            .propose(Changeset {
                ops: vec![ChangeOp::SetRaw {
                    note_id: "exact".into(),
                    raw: Some(raw.into()),
                }],
            })
            .unwrap();
        let commit = writer.commit(&proposal.change_id).unwrap();
        assert_eq!(fs::read_to_string(dir.join("exact.md")).unwrap(), raw);

        // SetRaw(None) deletes; revert of the delete restores the bytes.
        let proposal = writer
            .propose(Changeset {
                ops: vec![ChangeOp::SetRaw {
                    note_id: "exact".into(),
                    raw: None,
                }],
            })
            .unwrap();
        let delete = writer.commit(&proposal.change_id).unwrap();
        assert!(!dir.join("exact.md").exists());
        writer.revert(&delete.version_id).unwrap();
        assert_eq!(fs::read_to_string(dir.join("exact.md")).unwrap(), raw);
        let _ = commit;
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn audit_entry_origin_roundtrips_and_defaults_none() {
        let dir = temp_vault("origin");
        let mut writer = NoteWriter::new(&dir, PermissionScope::ReadWrite);
        writer.create_note("plain", "no origin", None).unwrap();
        writer.set_origin("sync");
        writer.create_note("synced", "with origin", None).unwrap();

        let audit = writer.read_audit(10).unwrap();
        assert_eq!(audit[0].origin.as_deref(), Some("sync"));
        assert_eq!(audit[1].origin, None);
        // Old log lines (no origin field) still parse: default() covered by
        // the entry written before set_origin.
        fs::remove_dir_all(&dir).ok();
    }
}
