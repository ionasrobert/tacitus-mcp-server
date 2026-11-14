use sha2::{Digest, Sha256};
use std::fmt::Write;

use crate::error::TacitusError;

/// Deterministic, stable id from a seed — the backbone of idempotency.
///
/// Identical to the TS `stableId`: `sha256(seed)` hex, first 16 chars, prefixed.
/// Same seed ⇒ same id across the TS and Rust engines.
pub fn stable_id(seed: &str, prefix: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(8) {
        let _ = write!(hex, "{byte:02x}");
    }
    format!("{prefix}_{hex}")
}

/// Why a note id can't be used as a vault-relative path, or None if it can.
///
/// A note id becomes `<vault>/<id>.md` on disk, so it must stay inside the
/// vault and outside Tacitus' own state: no absolute paths (`/x`, `C:/x`), no
/// `\` or NUL, no empty / `.` / `..` segments, and never under `.tacitus/`.
/// Identical rules in the TS engine (`assertNoteId`).
pub fn note_id_problem(id: &str) -> Option<&'static str> {
    if id.is_empty() {
        return Some("is empty");
    }
    if id.starts_with('/') {
        return Some("is an absolute path");
    }
    if id.contains('\\') || id.contains('\0') {
        return Some("contains a backslash or NUL");
    }
    let b = id.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Some("starts with a drive letter");
    }
    if id
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Some("has an empty, '.' or '..' segment");
    }
    if id.split('/').next() == Some(".tacitus") {
        return Some("points inside .tacitus/");
    }
    None
}

/// [`note_id_problem`] as a structured `INVALID_NOTE_ID` error.
pub fn validate_note_id(id: &str) -> Result<(), TacitusError> {
    match note_id_problem(id) {
        None => Ok(()),
        Some(problem) => Err(TacitusError::new(
            "INVALID_NOTE_ID",
            format!("Note id {id:?} {problem}."),
            "Use a vault-relative id like \"projects/launch\" (no leading /, no .. segments, not inside .tacitus).",
        )),
    }
}

/// True for ids Tacitus generates (`mem_…`, `v_…`, `chg_…`): `[A-Za-z0-9_-]+`.
/// Memory and version ids are file names, so anything else is refused before
/// it reaches the filesystem.
pub fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic_and_prefixed() {
        let a = stable_id("hello world", "mem");
        let b = stable_id("hello world", "mem");
        assert_eq!(a, b);
        assert!(a.starts_with("mem_"));
        assert_eq!(a.len(), 20); // "mem_" + 16 hex chars
    }

    #[test]
    fn different_seed_different_id() {
        assert_ne!(stable_id("a", "mem"), stable_id("b", "mem"));
    }

    #[test]
    fn note_ids_that_escape_the_vault_are_refused() {
        for bad in [
            "",
            "../x",
            "a/../../x",
            "/etc/passwd",
            "C:/x",
            "c:x",
            "a\\b",
            "a\0b",
            "a//b",
            "./a",
            "a/",
            ".tacitus/audit",
            ".tacitus",
        ] {
            assert!(note_id_problem(bad).is_some(), "should refuse {bad:?}");
            assert_eq!(validate_note_id(bad).unwrap_err().code, "INVALID_NOTE_ID");
        }
        for good in [
            "a",
            "projects/launch",
            "Meeting: notes",
            ".obsidian/x",
            "a.b/c..d",
            "daily/2026-11-14",
        ] {
            assert_eq!(note_id_problem(good), None, "should accept {good:?}");
        }
    }

    #[test]
    fn plain_ids_are_generated_shape_only() {
        assert!(is_plain_id(&stable_id("x", "mem")));
        assert!(is_plain_id("v_abc-1"));
        for bad in ["", "../x", "a/b", "a.b", "a b"] {
            assert!(!is_plain_id(bad), "{bad:?}");
        }
    }
}
