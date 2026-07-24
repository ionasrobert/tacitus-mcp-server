//! Vault manifest: tombstones with causal delete semantics.
//!
//! A delete records the note-doc's `StateVector` at delete time. At
//! materialization the item counts as deleted only if that vector covers
//! every op the doc currently holds — so an edit the deleter never saw
//! resurrects the note (edit wins), while re-deleting after seeing the
//! edits keeps it deleted. No wall clocks anywhere.

use std::fmt::Write as _;

use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, Map, ReadTxn, StateVector, Transact};

const TOMBSTONES: &str = "tombstones";

fn hex_encode(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

/// Union of two state vectors: per client, the max clock.
fn union(a: &StateVector, b: &StateVector) -> StateVector {
    let mut merged = a.clone();
    for (client, clock) in b.iter() {
        merged.set_max(*client, *clock);
    }
    merged
}

fn stored_tombstone<T: ReadTxn>(txn: &T, item: &str) -> Option<StateVector> {
    let map = txn.get_map(TOMBSTONES)?;
    let value = map.get(txn, item)?;
    let hex: String = value.cast().ok()?;
    let bytes = hex_decode(&hex)?;
    StateVector::decode_v1(&bytes).ok()
}

/// Record a tombstone for `item` at `doc_sv` (unioned with any prior
/// tombstone), returning the manifest update to broadcast.
pub fn record_delete(manifest: &Doc, item: &str, doc_sv: &StateVector) -> Vec<u8> {
    let map = manifest.get_or_insert_map(TOMBSTONES);
    let mut txn = manifest.transact_mut();
    let before = txn.state_vector();
    let merged = match stored_tombstone(&txn, item) {
        Some(prev) => union(&prev, doc_sv),
        None => doc_sv.clone(),
    };
    map.insert(&mut txn, item, hex_encode(&merged.encode_v1()));
    txn.encode_state_as_update_v2(&before)
}

/// True if the recorded tombstone for `item` covers `doc_sv` (item deleted).
/// No tombstone → not deleted.
pub fn covers(manifest: &Doc, item: &str, doc_sv: &StateVector) -> bool {
    let txn = manifest.transact();
    let Some(tomb) = stored_tombstone(&txn, item) else {
        return false;
    };
    doc_sv
        .iter()
        .all(|(client, clock)| tomb.get(client) >= *clock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{Transact, Update};

    fn apply(doc: &Doc, update: &[u8]) {
        let mut txn = doc.transact_mut();
        txn.apply_update(Update::decode_v2(update).unwrap())
            .unwrap();
    }

    #[test]
    fn no_tombstone_means_not_deleted() {
        let manifest = Doc::new();
        assert!(!covers(&manifest, "n:a", &StateVector::default()));
    }

    #[test]
    fn tombstone_covers_the_state_it_saw() {
        let manifest = Doc::new();
        let note = Doc::new();
        {
            use yrs::Text;
            let text = note.get_or_insert_text("c");
            let mut txn = note.transact_mut();
            text.insert(&mut txn, 0, "content");
        }
        let sv = note.transact().state_vector();
        let update = record_delete(&manifest, "n:a", &sv);
        assert!(!update.is_empty());
        assert!(covers(&manifest, "n:a", &sv));
    }

    #[test]
    fn unseen_edits_escape_the_tombstone() {
        let manifest = Doc::new();
        let note = Doc::new();
        {
            use yrs::Text;
            let text = note.get_or_insert_text("c");
            let mut txn = note.transact_mut();
            text.insert(&mut txn, 0, "v1");
        }
        let sv_at_delete = note.transact().state_vector();
        record_delete(&manifest, "n:a", &sv_at_delete);

        // An edit the deleter never saw
        {
            use yrs::Text;
            let text = note.get_or_insert_text("c");
            let mut txn = note.transact_mut();
            text.insert(&mut txn, 2, " plus unseen edit");
        }
        let sv_now = note.transact().state_vector();
        assert!(!covers(&manifest, "n:a", &sv_now), "edit wins — resurrect");
    }

    #[test]
    fn tombstone_updates_merge_across_replicas() {
        let a = Doc::new();
        let b = Doc::new();
        let sv = StateVector::default();
        let ua = record_delete(&a, "n:x", &sv);
        apply(&b, &ua);
        assert!(covers(&b, "n:x", &sv));
    }
}
