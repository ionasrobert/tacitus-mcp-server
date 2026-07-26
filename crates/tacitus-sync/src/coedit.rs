//! Ephemeral co-editing frames (collab-m3): yjs/yrs v2 text updates and
//! awareness (cursor) blobs for the note two peers have open together.
//! They ride the same relay frame type as presence but are sealed under
//! their own AAD domain (`"{vault_id}#coedit"`) — a 0.20 client fails the
//! AEAD check and skips them silently, the relay sees only ciphertext.
//! Durability comes from the checkpointed push tier (the relay log), never
//! from these frames: a lost frame is repaired by the next durable diff.

use serde::{Deserialize, Serialize};

use crate::protocol::b64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoeditKind {
    /// A CRDT text update (yjs/yrs update v2) for the note's doc.
    #[default]
    Update,
    /// A yjs awareness blob (cursors/selections) — never applied to docs,
    /// only forwarded to an open room.
    Awareness,
}

/// What travels inside the ciphertext. Everything except `v`/`device`/
/// `note_id` is `serde(default)` so the payload can grow without breaking
/// peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoeditPayload {
    pub v: u32,
    pub device: String,
    /// Sender's wall clock, ms since epoch (informational).
    #[serde(default)]
    pub ts: u64,
    /// UI-level note id ("projects/launch"), not the `n:`-prefixed doc key.
    pub note_id: String,
    #[serde(default)]
    pub kind: CoeditKind,
    #[serde(with = "b64", default)]
    pub data: Vec<u8>,
}

/// The AAD domain for co-edit blobs — cryptographically separate from both
/// sync updates (vault_id) and presence (`#presence`).
pub(crate) fn coedit_aad(vault_id: &str) -> String {
    format!("{vault_id}#coedit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrips_and_missing_fields_default() {
        let payload = CoeditPayload {
            v: 1,
            device: "dev_a".into(),
            ts: 42,
            note_id: "projects/launch".into(),
            kind: CoeditKind::Awareness,
            data: vec![1, 2, 255],
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"kind\":\"awareness\""));
        let back: CoeditPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back.note_id, "projects/launch");
        assert_eq!(back.kind, CoeditKind::Awareness);
        assert_eq!(back.data, vec![1, 2, 255]);

        // A minimal (or future) payload still parses — forward compat.
        let minimal: CoeditPayload =
            serde_json::from_str(r#"{"v":1,"device":"dev_x","note_id":"plan"}"#).unwrap();
        assert_eq!(minimal.kind, CoeditKind::Update);
        assert!(minimal.data.is_empty());
        assert_eq!(minimal.ts, 0);
    }

    #[test]
    fn aad_domain_is_distinct_from_presence_and_sync() {
        let vid = "ab".repeat(16);
        let aad = coedit_aad(&vid);
        assert!(aad.ends_with("#coedit"));
        assert_ne!(aad, vid);
        assert_ne!(aad, crate::presence::presence_aad(&vid));
    }
}
