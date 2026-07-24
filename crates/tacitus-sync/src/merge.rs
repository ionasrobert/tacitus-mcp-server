//! Bridge between snapshot diffs and CRDT operations: since Tacitus has no
//! file watcher, a local edit is observed as (old text, new text) at scan
//! time; `dissimilar` turns that into splices applied to the note's YText.
//! Two devices doing this independently always converge (yrs merge), so a
//! "3-way merge" here cannot fail and cannot lose edits.

use dissimilar::Chunk;
use yrs::{TextRef, TransactionMut};

/// Apply the minimal splices that turn `old` into `new` onto the shared text.
/// The doc must use byte offsets (`OffsetKind::Bytes`); dissimilar chunks are
/// valid `&str` slices, so every offset lands on a char boundary.
pub fn apply_splices(text: &TextRef, txn: &mut TransactionMut, old: &str, new: &str) {
    use yrs::Text;
    let mut pos = 0u32;
    for chunk in dissimilar::diff(old, new) {
        match chunk {
            Chunk::Equal(s) => pos += s.len() as u32,
            Chunk::Delete(s) => {
                text.remove_range(txn, pos, s.len() as u32);
            }
            Chunk::Insert(s) => {
                text.insert(txn, pos, s);
                pos += s.len() as u32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::updates::decoder::Decode;
    use yrs::{Doc, GetString, Options, ReadTxn, Text, Transact, Update};

    fn byte_doc() -> Doc {
        Doc::with_options(Options {
            offset_kind: yrs::OffsetKind::Bytes,
            ..Default::default()
        })
    }

    fn text_of(doc: &Doc) -> String {
        let text = doc.get_or_insert_text("c");
        let txn = doc.transact();
        text.get_string(&txn)
    }

    fn seed(doc: &Doc, content: &str) {
        let text = doc.get_or_insert_text("c");
        let mut txn = doc.transact_mut();
        text.insert(&mut txn, 0, content);
    }

    fn edit(doc: &Doc, old: &str, new: &str) -> Vec<u8> {
        let text = doc.get_or_insert_text("c");
        let mut txn = doc.transact_mut();
        let before = txn.state_vector();
        apply_splices(&text, &mut txn, old, new);
        txn.encode_state_as_update_v2(&before)
    }

    fn apply(doc: &Doc, update: &[u8]) {
        let mut txn = doc.transact_mut();
        txn.apply_update(Update::decode_v2(update).unwrap())
            .unwrap();
    }

    /// Clone a doc's full state into a replica with its own client id.
    fn replica(doc: &Doc) -> Doc {
        let full = doc
            .transact()
            .encode_state_as_update_v2(&yrs::StateVector::default());
        let other = byte_doc();
        apply(&other, &full);
        other
    }

    #[test]
    fn splices_turn_old_text_into_new_text() {
        let doc = byte_doc();
        seed(&doc, "Hello world, this is Tacitus.\n");
        edit(
            &doc,
            "Hello world, this is Tacitus.\n",
            "Hello brave new world, this is still Tacitus.\n",
        );
        assert_eq!(
            text_of(&doc),
            "Hello brave new world, this is still Tacitus.\n"
        );
    }

    #[test]
    fn concurrent_disjoint_edits_merge_both() {
        let a = byte_doc();
        seed(&a, "# Title\n\nfirst paragraph\n\nlast paragraph\n");
        let b = replica(&a);

        let ua = edit(
            &a,
            "# Title\n\nfirst paragraph\n\nlast paragraph\n",
            "# Title\n\nfirst paragraph EDITED BY A\n\nlast paragraph\n",
        );
        let ub = edit(
            &b,
            "# Title\n\nfirst paragraph\n\nlast paragraph\n",
            "# Title\n\nfirst paragraph\n\nlast paragraph EDITED BY B\n",
        );
        apply(&a, &ub);
        apply(&b, &ua);

        assert_eq!(text_of(&a), text_of(&b), "replicas must converge");
        assert!(text_of(&a).contains("EDITED BY A"));
        assert!(text_of(&a).contains("EDITED BY B"));
    }

    #[test]
    fn concurrent_same_region_edits_preserve_all_characters() {
        let a = byte_doc();
        seed(&a, "The color of the sky.\n");
        let b = replica(&a);

        let ua = edit(&a, "The color of the sky.\n", "The colAAAor of the sky.\n");
        let ub = edit(&b, "The color of the sky.\n", "The colBBBor of the sky.\n");
        apply(&a, &ub);
        apply(&b, &ua);

        assert_eq!(text_of(&a), text_of(&b));
        assert!(text_of(&a).contains("AAA"), "A's insert survives");
        assert!(text_of(&a).contains("BBB"), "B's insert survives");
    }

    #[test]
    fn frontmatter_disjoint_key_edits_merge_cleanly() {
        let original = "---\ntitle: Draft\nstatus: active\n---\n\nBody.\n";
        let a = byte_doc();
        seed(&a, original);
        let b = replica(&a);

        let ua = edit(
            &a,
            original,
            "---\ntitle: Final Title\nstatus: active\n---\n\nBody.\n",
        );
        let ub = edit(
            &b,
            original,
            "---\ntitle: Draft\nstatus: done\n---\n\nBody.\n",
        );
        apply(&a, &ub);
        apply(&b, &ua);

        assert_eq!(text_of(&a), text_of(&b));
        assert!(text_of(&a).contains("title: Final Title"));
        assert!(text_of(&a).contains("status: done"));
    }

    #[test]
    fn remote_update_apply_is_idempotent() {
        let a = byte_doc();
        seed(&a, "stable\n");
        let b = replica(&a);
        let u = edit(&a, "stable\n", "stable and grown\n");

        apply(&b, &u);
        let once = text_of(&b);
        apply(&b, &u);
        assert_eq!(text_of(&b), once);
    }
}
