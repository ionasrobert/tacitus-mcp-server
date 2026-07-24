//! Per-item CRDT documents, persisted compacted under `.tacitus/sync/docs/`.
//!
//! One YDoc per item, one YText `"c"` holding the entire raw file — so a
//! materialized doc reproduces the file byte-for-byte. Files are named by a
//! reversible percent-encoding of the item key (so the store can enumerate
//! its items after a restart), plus `manifest.yrs` for the tombstone doc.
//!
//! Deterministic bootstrap: a doc created from pre-existing content uses a
//! client id derived from that content, as a single insert op. Two devices
//! bootstrapping identical files therefore emit identical updates, which
//! dedup to one copy on first sync. The bootstrap doc is discarded
//! immediately — live docs always carry fresh random client ids, so later
//! edits can never collide.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use yrs::updates::decoder::Decode;
use yrs::{Doc, GetString, Options, ReadTxn, StateVector, Text, Transact, Update};

use crate::manifest;
use crate::merge::apply_splices;

pub const MANIFEST_KEY: &str = "manifest";
const MANIFEST_FILE: &str = "manifest.yrs";

pub struct DocStore {
    docs_dir: PathBuf,
    docs: HashMap<String, Doc>,
    manifest: Doc,
}

/// A doc that measures text offsets in bytes (dissimilar splices are byte
/// ranges on valid char boundaries).
fn byte_doc() -> Doc {
    Doc::with_options(Options {
        offset_kind: yrs::OffsetKind::Bytes,
        ..Default::default()
    })
}

/// yjs-compatible 53-bit client id derived from content — the deterministic
/// bootstrap identity.
fn deterministic_client_id(content: &str) -> u64 {
    let digest = Sha256::digest(content.as_bytes());
    let mut id = 0u64;
    for byte in digest.iter().take(8) {
        id = (id << 8) | u64::from(*byte);
    }
    id & ((1 << 53) - 1)
}

/// Reversible flat filename for an item key: percent-encode `%`, `:`, `/`.
fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 8);
    for c in key.chars() {
        match c {
            '%' => out.push_str("%25"),
            ':' => out.push_str("%3A"),
            '/' => out.push_str("%2F"),
            _ => out.push(c),
        }
    }
    out.push_str(".yrs");
    out
}

fn decode_key(file_name: &str) -> Option<String> {
    let stem = file_name.strip_suffix(".yrs")?;
    let mut out = String::with_capacity(stem.len());
    let mut chars = stem.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hi = chars.next()?;
            let lo = chars.next()?;
            let byte = u8::from_str_radix(&format!("{hi}{lo}"), 16).ok()?;
            out.push(byte as char);
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn full_state(doc: &Doc) -> Vec<u8> {
    doc.transact()
        .encode_state_as_update_v2(&StateVector::default())
}

fn apply_update_bytes(doc: &Doc, update: &[u8]) -> io::Result<()> {
    let parsed = Update::decode_v2(update)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut txn = doc.transact_mut();
    txn.apply_update(parsed)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("yrs.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

impl DocStore {
    pub fn open(sync_dir: &Path) -> io::Result<Self> {
        let docs_dir = sync_dir.join("docs");
        fs::create_dir_all(&docs_dir)?;
        let manifest = Doc::new();
        match fs::read(docs_dir.join(MANIFEST_FILE)) {
            Ok(bytes) => apply_update_bytes(&manifest, &bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(Self {
            docs_dir,
            docs: HashMap::new(),
            manifest,
        })
    }

    fn doc_path(&self, item: &str) -> PathBuf {
        self.docs_dir.join(encode_key(item))
    }

    /// Load from cache or disk; None if the item has no doc yet.
    fn load(&mut self, item: &str) -> io::Result<Option<Doc>> {
        if let Some(doc) = self.docs.get(item) {
            return Ok(Some(doc.clone()));
        }
        match fs::read(self.doc_path(item)) {
            Ok(bytes) => {
                let doc = byte_doc();
                apply_update_bytes(&doc, &bytes)?;
                self.docs.insert(item.to_string(), doc.clone());
                Ok(Some(doc))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn persist(&self, item: &str, doc: &Doc) -> io::Result<()> {
        atomic_write(&self.doc_path(item), &full_state(doc))
    }

    fn persist_manifest(&self) -> io::Result<()> {
        atomic_write(
            &self.docs_dir.join(MANIFEST_FILE),
            &full_state(&self.manifest),
        )
    }

    /// Fold a local snapshot of the item into its doc; returns the update to
    /// broadcast (a minimal splice diff, or a deterministic bootstrap for a
    /// brand-new item).
    pub fn apply_local_text(&mut self, item: &str, new_text: &str) -> io::Result<Vec<u8>> {
        if let Some(doc) = self.load(item)? {
            let text = doc.get_or_insert_text("c");
            let old = {
                let txn = doc.transact();
                text.get_string(&txn)
            };
            if old == new_text {
                return Ok(Vec::new());
            }
            let update = {
                let mut txn = doc.transact_mut();
                let before = txn.state_vector();
                apply_splices(&text, &mut txn, &old, new_text);
                txn.encode_state_as_update_v2(&before)
            };
            self.persist(item, &doc)?;
            return Ok(update);
        }

        // New item: deterministic bootstrap, then reload into a fresh doc so
        // future edits use a random client id.
        let update = {
            let boot = Doc::with_options(Options {
                offset_kind: yrs::OffsetKind::Bytes,
                client_id: yrs::block::ClientID::new(deterministic_client_id(new_text)),
                ..Default::default()
            });
            let text = boot.get_or_insert_text("c");
            {
                let mut txn = boot.transact_mut();
                text.insert(&mut txn, 0, new_text);
            }
            full_state(&boot)
        };
        let doc = byte_doc();
        apply_update_bytes(&doc, &update)?;
        self.persist(item, &doc)?;
        self.docs.insert(item.to_string(), doc);
        Ok(update)
    }

    /// Record a causal tombstone for the item; returns the manifest update.
    pub fn record_delete(&mut self, item: &str) -> io::Result<Vec<u8>> {
        let sv = match self.load(item)? {
            Some(doc) => doc.transact().state_vector(),
            None => StateVector::default(),
        };
        let update = manifest::record_delete(&self.manifest, item, &sv);
        self.persist_manifest()?;
        Ok(update)
    }

    /// Apply a remote update to an item doc or (target == "manifest") the
    /// manifest. Idempotent.
    pub fn apply_remote(&mut self, target: &str, update: &[u8]) -> io::Result<()> {
        if target == MANIFEST_KEY {
            apply_update_bytes(&self.manifest, update)?;
            return self.persist_manifest();
        }
        let doc = match self.load(target)? {
            Some(doc) => doc,
            None => {
                let doc = byte_doc();
                self.docs.insert(target.to_string(), doc.clone());
                doc
            }
        };
        apply_update_bytes(&doc, update)?;
        self.persist(target, &doc)
    }

    /// The item's current text — or None when it has no doc, or its tombstone
    /// causally covers everything the doc holds (deleted, nothing to
    /// resurrect).
    pub fn materialize(&mut self, item: &str) -> io::Result<Option<String>> {
        let Some(doc) = self.load(item)? else {
            return Ok(None);
        };
        let sv = doc.transact().state_vector();
        if manifest::covers(&self.manifest, item, &sv) {
            return Ok(None);
        }
        let text = doc.get_or_insert_text("c");
        let txn = doc.transact();
        Ok(Some(text.get_string(&txn)))
    }

    /// Every item this store has a doc for (from disk — survives restarts).
    pub fn known_items(&self) -> io::Result<Vec<String>> {
        let mut items = Vec::new();
        for entry in fs::read_dir(&self.docs_dir)?.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == MANIFEST_FILE || !name.ends_with(".yrs") {
                continue;
            }
            if let Some(key) = decode_key(name) {
                items.push(key);
            }
        }
        items.sort();
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_store(tag: &str) -> (PathBuf, DocStore) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-docs-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        let store = DocStore::open(&dir).unwrap();
        (dir, store)
    }

    /// Ship (target, update) pairs from one store to another.
    fn ship(to: &mut DocStore, updates: &[(&str, Vec<u8>)]) {
        for (target, update) in updates {
            if !update.is_empty() {
                to.apply_remote(target, update).unwrap();
            }
        }
    }

    #[test]
    fn local_edit_becomes_minimal_splice_update() {
        let (dir, mut store) = temp_store("splice");
        let body = "A reasonably sized note about the launch plan. ".repeat(20);
        let original = format!("# Launch\n\n{body}\n");
        let edited = format!("# Launch\n\n{body}\nOne appended line.\n");
        let boot = store.apply_local_text("n:a", &original).unwrap();
        let edit = store.apply_local_text("n:a", &edited).unwrap();
        assert!(!edit.is_empty());
        assert!(
            edit.len() < boot.len() / 4,
            "a splice update carries the edit, not the whole note ({} vs {})",
            edit.len(),
            boot.len()
        );
        assert_eq!(store.materialize("n:a").unwrap().unwrap(), edited);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn materialize_reproduces_exact_bytes() {
        let (dir, mut store) = temp_store("bytes");
        let content = "---\ntitle: Ünïcode ✓\ntags: [a, b]\n---\n\nBody with trailing spaces  \nand a final newline\n\n";
        store.apply_local_text("n:exact", content).unwrap();
        assert_eq!(store.materialize("n:exact").unwrap().unwrap(), content);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn doc_store_roundtrip_compacts_state() {
        let (dir, mut store) = temp_store("roundtrip");
        store.apply_local_text("n:r", "v1\n").unwrap();
        store.apply_local_text("n:r", "v1 v2\n").unwrap();
        store.apply_local_text("n:r", "v1 v2 v3\n").unwrap();
        drop(store);

        let mut reopened = DocStore::open(&dir).unwrap();
        assert_eq!(reopened.materialize("n:r").unwrap().unwrap(), "v1 v2 v3\n");
        assert_eq!(reopened.known_items().unwrap(), vec!["n:r"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_create_identical_content_dedupes() {
        let (da, mut a) = temp_store("dedup-a");
        let (db, mut b) = temp_store("dedup-b");
        let content = "# Same note on both devices\n";
        let ua = a.apply_local_text("n:same", content).unwrap();
        let ub = b.apply_local_text("n:same", content).unwrap();
        assert_eq!(ua, ub, "identical content must bootstrap identically");

        ship(&mut b, &[("n:same", ua)]);
        ship(&mut a, &[("n:same", ub)]);
        assert_eq!(a.materialize("n:same").unwrap().unwrap(), content);
        assert_eq!(b.materialize("n:same").unwrap().unwrap(), content);
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn create_create_divergent_content_keeps_both_texts_flagged() {
        let (da, mut a) = temp_store("div-a");
        let (db, mut b) = temp_store("div-b");
        let ua = a
            .apply_local_text("n:clash", "Written on device A\n")
            .unwrap();
        let ub = b
            .apply_local_text("n:clash", "Written on device B\n")
            .unwrap();

        ship(&mut b, &[("n:clash", ua)]);
        ship(&mut a, &[("n:clash", ub)]);
        let ta = a.materialize("n:clash").unwrap().unwrap();
        let tb = b.materialize("n:clash").unwrap().unwrap();
        assert_eq!(ta, tb, "replicas converge deterministically");
        assert!(ta.contains("device A"), "no lost edits");
        assert!(ta.contains("device B"), "no lost edits");
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn delete_vs_concurrent_edit_edit_wins_resurrects() {
        let (da, mut a) = temp_store("del-edit-a");
        let (db, mut b) = temp_store("del-edit-b");
        let boot = a.apply_local_text("n:x", "original\n").unwrap();
        ship(&mut b, &[("n:x", boot)]);

        // A deletes; B edits concurrently (unseen by A).
        let del = a.record_delete("n:x").unwrap();
        let edit = b
            .apply_local_text("n:x", "original plus B's edit\n")
            .unwrap();

        ship(&mut b, &[(MANIFEST_KEY, del)]);
        ship(&mut a, &[("n:x", edit)]);

        assert_eq!(
            a.materialize("n:x").unwrap().as_deref(),
            Some("original plus B's edit\n"),
            "edit wins on A"
        );
        assert_eq!(
            b.materialize("n:x").unwrap().as_deref(),
            Some("original plus B's edit\n"),
            "edit wins on B"
        );
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn delete_after_seen_edits_stays_deleted() {
        let (da, mut a) = temp_store("del-seen-a");
        let (db, mut b) = temp_store("del-seen-b");
        let boot = a.apply_local_text("n:y", "v1\n").unwrap();
        ship(&mut b, &[("n:y", boot)]);
        let edit = b.apply_local_text("n:y", "v1 edited\n").unwrap();
        ship(&mut a, &[("n:y", edit)]);

        // A deletes AFTER seeing B's edit.
        let del = a.record_delete("n:y").unwrap();
        ship(&mut b, &[(MANIFEST_KEY, del)]);

        assert_eq!(a.materialize("n:y").unwrap(), None);
        assert_eq!(b.materialize("n:y").unwrap(), None);
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn rename_becomes_delete_plus_create_items() {
        let (dir, mut store) = temp_store("rename");
        store.apply_local_text("n:old-name", "content\n").unwrap();
        // A rename surfaces at scan level as delete(old) + create(new).
        store.record_delete("n:old-name").unwrap();
        store.apply_local_text("n:new-name", "content\n").unwrap();

        assert_eq!(store.materialize("n:old-name").unwrap(), None);
        assert_eq!(
            store.materialize("n:new-name").unwrap().as_deref(),
            Some("content\n")
        );
        fs::remove_dir_all(&dir).ok();
    }
}
