//! Rename with automatic wikilink rewriting — as ONE atomic changeset:
//! create the new note (self-links rewritten), update every note whose links
//! resolve to the old id (alias/heading/block parts preserved, basename and
//! case-insensitive references included), delete the old note. Committed
//! through the transactional `NoteWriter`, the whole rename is all-or-nothing,
//! versioned as a single snapshot, and undone by a single revert.
//!
//! Pure: this module only BUILDS the changeset from an index snapshot; the
//! caller proposes + commits it (no lock is ever held across a write).

use serde_yaml::Value;

use super::index::VaultIndex;
use super::types::{Note, WikiLink};
use super::write::{ChangeOp, Changeset};
use crate::error::TacitusError;

/// Rebuild a wikilink's raw text with a new target, keeping heading/block and
/// alias parts intact.
fn rebuild_link(link: &WikiLink, new_target: &str) -> String {
    let mut s = String::from("[[");
    s.push_str(new_target);
    if let Some(heading) = &link.heading {
        s.push('#');
        s.push_str(heading);
    }
    if let Some(block) = &link.block {
        s.push_str("#^");
        s.push_str(block);
    }
    if let Some(alias) = &link.alias {
        s.push('|');
        s.push_str(alias);
    }
    s.push_str("]]");
    s
}

/// The note's content with every link that resolves to `from` retargeted to
/// `to`; None if nothing needed rewriting.
fn rewritten_content(index: &VaultIndex, note: &Note, from: &str, to: &str) -> Option<String> {
    let mut content = note.content.clone();
    let mut changed = false;
    for link in &note.links {
        if index.resolve(&link.target).map(|n| n.id.as_str()) == Some(from) {
            let new_raw = rebuild_link(link, to);
            if new_raw != link.raw {
                content = content.replace(&link.raw, &new_raw);
                changed = true;
            }
        }
    }
    changed.then_some(content)
}

/// Build the atomic changeset that renames `from` to `to` and retargets every
/// affected wikilink. Commit it via `NoteWriter::propose` + `commit`.
pub fn rename_note_ops(
    index: &VaultIndex,
    from: &str,
    to: &str,
) -> Result<Changeset, TacitusError> {
    let to = to.trim();
    if to.is_empty() || to == from {
        return Err(TacitusError::new(
            "INVALID_INPUT",
            format!("Cannot rename {from:?} to {to:?}."),
            "Pass a different, non-empty note id.",
        ));
    }
    let note = index.get(from).ok_or_else(|| {
        TacitusError::new(
            "NOTE_NOT_FOUND",
            format!("No note with id {from:?}."),
            "Check the id with list_notes.",
        )
    })?;
    if index.get(to).is_some() {
        return Err(TacitusError::new(
            "CONFLICT",
            format!("Note {to:?} already exists."),
            "Choose a different target id.",
        ));
    }

    let mut ops = Vec::new();
    // The moved note itself — with its own self-links retargeted.
    let moved_content =
        rewritten_content(index, note, from, to).unwrap_or_else(|| note.content.clone());
    let frontmatter = match &note.frontmatter {
        Value::Mapping(m) if m.is_empty() => None,
        fm => Some(fm.clone()),
    };
    ops.push(ChangeOp::Create {
        note_id: to.to_string(),
        content: moved_content,
        frontmatter,
    });
    // Every other note whose links resolve to the old id.
    for linking in index.backlinks(from) {
        if let Some(content) = rewritten_content(index, linking, from, to) {
            ops.push(ChangeOp::Update {
                note_id: linking.id.clone(),
                content: Some(content),
                frontmatter: None,
            });
        }
    }
    ops.push(ChangeOp::Delete {
        note_id: from.to_string(),
    });
    Ok(Changeset { ops })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::write::{NoteWriter, PermissionScope};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_vault() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-rename-{nanos}"));
        fs::create_dir_all(dir.join("projects")).unwrap();
        fs::write(
            dir.join("projects/launch.md"),
            "---\ntitle: Launch\npriority: 3\n---\nSee [[projects/launch]] self-link and [[ideas]].\n",
        )
        .unwrap();
        fs::write(
            dir.join("a.md"),
            "Full id [[projects/launch]] and alias [[projects/launch|the launch]].\n",
        )
        .unwrap();
        fs::write(
            dir.join("b.md"),
            "Basename case-insensitive [[Launch#Timeline]] link.\n",
        )
        .unwrap();
        fs::write(dir.join("ideas.md"), "# Ideas\nUnrelated [[a]].\n").unwrap();
        dir
    }

    fn commit_rename(dir: &PathBuf, from: &str, to: &str) -> (Arc<Mutex<VaultIndex>>, String) {
        let index = Arc::new(Mutex::new(VaultIndex::build(dir).unwrap()));
        let mut writer = NoteWriter::with_index(dir, PermissionScope::ReadWrite, index.clone());
        // Snapshot under the lock, commit with it released.
        let changeset = {
            let idx = index.lock().unwrap();
            rename_note_ops(&idx, from, to).unwrap()
        };
        let proposal = writer.propose(changeset).unwrap();
        let version = writer.commit(&proposal.change_id).unwrap().version_id;
        (index, version)
    }

    #[test]
    fn rename_moves_the_note_and_rewrites_every_link_shape() {
        let dir = make_vault();
        let (index, _) = commit_rename(&dir, "projects/launch", "projects/rocket");

        assert!(!dir.join("projects/launch.md").exists());
        let moved = fs::read_to_string(dir.join("projects/rocket.md")).unwrap();
        assert!(moved.contains("priority: 3")); // typed frontmatter preserved
        assert!(moved.contains("[[projects/rocket]] self-link")); // self-link retargeted

        let a = fs::read_to_string(dir.join("a.md")).unwrap();
        assert!(a.contains("[[projects/rocket]]"));
        assert!(a.contains("[[projects/rocket|the launch]]")); // alias kept

        let b = fs::read_to_string(dir.join("b.md")).unwrap();
        assert!(b.contains("[[projects/rocket#Timeline]]")); // heading kept, basename resolved

        // Live index reflects everything: backlinks now point at the new id.
        let idx = index.lock().unwrap();
        assert!(idx.get("projects/launch").is_none());
        let backlinks: Vec<&str> = idx
            .backlinks("projects/rocket")
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert!(backlinks.contains(&"a") && backlinks.contains(&"b"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_revert_undoes_the_entire_rename() {
        let dir = make_vault();
        let (index, version) = commit_rename(&dir, "projects/launch", "projects/rocket");
        {
            let writer = NoteWriter::with_index(&dir, PermissionScope::ReadWrite, index.clone());
            writer.revert(&version).unwrap();
        }
        assert!(dir.join("projects/launch.md").exists());
        assert!(!dir.join("projects/rocket.md").exists());
        assert!(fs::read_to_string(dir.join("a.md"))
            .unwrap()
            .contains("[[projects/launch|the launch]]"));
        assert!(fs::read_to_string(dir.join("b.md"))
            .unwrap()
            .contains("[[Launch#Timeline]]"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_guards_conflict_missing_and_noop() {
        let dir = make_vault();
        let index = VaultIndex::build(&dir).unwrap();
        assert_eq!(
            rename_note_ops(&index, "projects/launch", "ideas")
                .unwrap_err()
                .code,
            "CONFLICT"
        );
        assert_eq!(
            rename_note_ops(&index, "nope", "x").unwrap_err().code,
            "NOTE_NOT_FOUND"
        );
        assert_eq!(
            rename_note_ops(&index, "ideas", "ideas").unwrap_err().code,
            "INVALID_INPUT"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
