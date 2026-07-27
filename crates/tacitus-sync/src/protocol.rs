//! The wire protocol: JSON text frames over a WebSocket, blobs as base64.
//! The relay never parses a blob — it assigns sequence numbers, appends to a
//! per-vault log, and fans updates out to the vault's other connections.

use serde::{Deserialize, Serialize};

/// Base64 (de)serialization for binary blobs inside JSON frames.
pub(crate) mod b64 {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        STANDARD.encode(bytes).serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

/// Capability advertised in `hello.caps`/`welcome.caps` when a side speaks
/// the ephemeral presence extension. New message *variants* are parse errors
/// for old peers, so anything beyond the 0.19 set must be caps-negotiated.
pub const CAP_PRESENCE: &str = "presence";

/// Capability for log compaction: a caught-up client may push a sealed
/// full-state snapshot (`compact`); the relay truncates the log beneath it
/// and serves the snapshot as the first backlog frame (`snapshot`) to any
/// capable client whose cursor is below it. Clients WITHOUT this cap that
/// hello below the snapshot get `err {code:"compacted"}` — an honest fatal
/// error instead of silent divergence.
pub const CAP_COMPACT: &str = "compact";

/// Capability for CHUNKED compaction (strict superset of `compact`): a
/// snapshot too big for one `compact` frame travels as an ordered series of
/// separately-sealed `compact_part` frames, and is served back the same way
/// (`snapshot_part`). Single-part snapshots keep the legacy frames, so a
/// vault that fits 32 MiB behaves byte-identically to 0.22. A compact-only
/// client below a MULTI-part snapshot gets `err {code:"compacted"}` — the
/// same honest fatal 0.21 clients get below any snapshot.
pub const CAP_COMPACT2: &str = "compact2";

/// Relay refusals of a compaction offer. Advisory by design (the log is
/// untouched, the connection survives), so the live driver must not die on
/// them — losing a compaction race is another device doing our job.
/// `"compacted"` is deliberately NOT here: that one is fatal.
#[cfg_attr(not(feature = "client"), allow(dead_code))]
pub(crate) const COMPACT_REFUSALS: [&str; 6] = [
    "compact_stale",
    "compact_ahead",
    "snapshot_too_large",
    "compact_part_too_large",
    "compact_too_large",
    "compact_part_order",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        vault_id: String,
        token: String,
        since_seq: u64,
        /// Extensions this client understands (absent on 0.19 → empty).
        #[serde(default)]
        caps: Vec<String>,
    },
    Push {
        #[serde(with = "b64")]
        blob: Vec<u8>,
    },
    /// Ephemeral: never logged, no seq, no ack — relay fans it out to the
    /// vault's presence-capable connections and forgets it.
    Presence {
        #[serde(with = "b64")]
        blob: Vec<u8>,
    },
    /// Compaction offer: `blob` is a sealed full-state snapshot covering the
    /// log up to `upto_seq` (the pusher's cursor). The relay truncates
    /// entries `<= upto_seq` and keeps serving seqs unchanged. The covered
    /// seq ALSO travels inside the ciphertext (`SyncPayload.snapshot_upto`)
    /// so a hostile relay can't forge coverage claims.
    Compact {
        upto_seq: u64,
        #[serde(with = "b64")]
        blob: Vec<u8>,
    },
    /// One part of a chunked compaction offer, sent in order `0..of-1` on
    /// one connection; the relay stages parts and commits on the last,
    /// replying `compacted`. Only sent when the relay advertised
    /// `compact2`. Each part is independently sealed; its set membership
    /// travels inside the ciphertext (`SyncPayload.snapshot_part`).
    CompactPart {
        upto_seq: u64,
        idx: u32,
        of: u32,
        #[serde(with = "b64")]
        blob: Vec<u8>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        latest_seq: u64,
        /// Extensions the relay supports (absent on 0.19 → empty). Clients
        /// must not send extension frames the relay didn't advertise.
        #[serde(default)]
        caps: Vec<String>,
        /// Current size of the vault's log on disk (absent pre-0.22 → 0).
        /// The compaction trigger: a caught-up client compacts when this
        /// crosses its threshold.
        #[serde(default)]
        log_bytes: u64,
    },
    Update {
        seq: u64,
        #[serde(with = "b64")]
        blob: Vec<u8>,
    },
    Ack {
        seq: u64,
    },
    Err {
        code: String,
        msg: String,
    },
    Presence {
        #[serde(with = "b64")]
        blob: Vec<u8>,
    },
    /// The compacted prefix of the log as one sealed full-state blob. Served
    /// before the tail to capable clients whose `since_seq` is below the
    /// snapshot. The client trusts only the seq INSIDE the ciphertext, not
    /// this cleartext `upto_seq` (the relay controls the latter).
    Snapshot {
        upto_seq: u64,
        #[serde(with = "b64")]
        blob: Vec<u8>,
    },
    /// One part of a chunked snapshot, served in place of `snapshot` (and
    /// only to compact2 connections) when the stored snapshot is
    /// multi-part. ALL cleartext fields are the relay's claim — the client
    /// trusts only the sealed `SyncPayload.snapshot_part` inside, and
    /// advances its cursor only once a complete consistent set applied.
    SnapshotPart {
        upto_seq: u64,
        idx: u32,
        of: u32,
        #[serde(with = "b64")]
        blob: Vec<u8>,
    },
    /// Reply to a `compact` push: the log was truncated up to `upto_seq`.
    Compacted {
        upto_seq: u64,
    },
}

// Used by the client drivers (feature "client"); tests exercise it always.
#[cfg_attr(not(feature = "client"), allow(dead_code))]
const KNOWN_SERVER_TAGS: [&str; 8] = [
    "welcome",
    "update",
    "ack",
    "err",
    "presence",
    "snapshot",
    "snapshot_part",
    "compacted",
];

/// Tolerant frame parsing for the client drivers: `Ok(None)` = a well-formed
/// frame with a tag from a FUTURE protocol version (skip it — don't kill the
/// session); `Err` = genuinely malformed, or a known tag with bad fields.
#[cfg_attr(not(feature = "client"), allow(dead_code))]
pub(crate) fn parse_server_msg(text: &str) -> Result<Option<ServerMsg>, serde_json::Error> {
    match serde_json::from_str::<ServerMsg>(text) {
        Ok(msg) => Ok(Some(msg)),
        Err(e) => {
            let value: serde_json::Value = serde_json::from_str(text)?;
            match value.get("t").and_then(|t| t.as_str()) {
                Some(tag) if !KNOWN_SERVER_TAGS.contains(&tag) => Ok(None),
                _ => Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_frames_without_caps_still_parse_with_empty_default() {
        // A 0.19 peer emits no "caps" field — both directions must keep
        // parsing (this is the whole backward-compat contract).
        let hello: ClientMsg =
            serde_json::from_str(r#"{"t":"hello","vault_id":"ab","token":"tk","since_seq":3}"#)
                .unwrap();
        match hello {
            ClientMsg::Hello {
                caps, since_seq, ..
            } => {
                assert!(caps.is_empty());
                assert_eq!(since_seq, 3);
            }
            other => panic!("expected hello, got {other:?}"),
        }
        let welcome: ServerMsg = serde_json::from_str(r#"{"t":"welcome","latest_seq":9}"#).unwrap();
        match welcome {
            ServerMsg::Welcome {
                caps,
                latest_seq,
                log_bytes,
            } => {
                assert!(caps.is_empty());
                assert_eq!(latest_seq, 9);
                // Pre-0.22 relays don't send log_bytes — 0 means "unknown",
                // which never crosses a compaction threshold.
                assert_eq!(log_bytes, 0);
            }
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    #[test]
    fn compact_and_snapshot_frames_roundtrip_json() {
        let compact = ClientMsg::Compact {
            upto_seq: 12,
            blob: vec![1, 2, 255],
        };
        let json = serde_json::to_string(&compact).unwrap();
        assert!(json.contains("\"t\":\"compact\""));
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(back, ClientMsg::Compact { upto_seq: 12, blob } if blob == vec![1, 2, 255])
        );

        let snapshot = ServerMsg::Snapshot {
            upto_seq: 12,
            blob: vec![9, 9],
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("\"t\":\"snapshot\""));
        let back: ServerMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ServerMsg::Snapshot { upto_seq: 12, blob } if blob == vec![9, 9]));

        let done = ServerMsg::Compacted { upto_seq: 12 };
        let json = serde_json::to_string(&done).unwrap();
        assert!(json.contains("\"t\":\"compacted\""));
        let back: ServerMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ServerMsg::Compacted { upto_seq: 12 }));
    }

    #[test]
    fn snapshot_and_compacted_are_known_tags_with_strict_fields() {
        // Now that these tags are known, malformed instances must be real
        // errors — not tolerant "future tag" skips.
        assert!(parse_server_msg(r#"{"t":"snapshot","upto_seq":"NaN","blob":""}"#).is_err());
        assert!(parse_server_msg(r#"{"t":"compacted"}"#).is_err());
        assert!(parse_server_msg(r#"{"t":"snapshot_part","upto_seq":1,"idx":0,"blob":""}"#).is_err());
        assert!(matches!(
            parse_server_msg(r#"{"t":"compacted","upto_seq":4}"#),
            Ok(Some(ServerMsg::Compacted { upto_seq: 4 }))
        ));
    }

    #[test]
    fn compact_part_and_snapshot_part_frames_roundtrip_json() {
        let part = ClientMsg::CompactPart {
            upto_seq: 12,
            idx: 1,
            of: 3,
            blob: vec![1, 2, 255],
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("\"t\":\"compact_part\""));
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            ClientMsg::CompactPart { upto_seq: 12, idx: 1, of: 3, blob } if blob == vec![1, 2, 255]
        ));

        let part = ServerMsg::SnapshotPart {
            upto_seq: 12,
            idx: 2,
            of: 3,
            blob: vec![9, 9],
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("\"t\":\"snapshot_part\""));
        let back: ServerMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            ServerMsg::SnapshotPart { upto_seq: 12, idx: 2, of: 3, blob } if blob == vec![9, 9]
        ));
    }

    #[test]
    fn presence_frames_roundtrip_with_b64_blobs() {
        let json = serde_json::to_string(&ClientMsg::Presence {
            blob: vec![9, 8, 255],
        })
        .unwrap();
        assert!(json.contains("\"t\":\"presence\""));
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ClientMsg::Presence { blob } if blob == vec![9, 8, 255]));

        let json = serde_json::to_string(&ServerMsg::Presence { blob: vec![1] }).unwrap();
        let back: ServerMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ServerMsg::Presence { blob } if blob == vec![1]));
    }

    #[test]
    fn parse_server_msg_skips_unknown_tags_but_rejects_malformed() {
        // A future protocol tag must not kill the session…
        assert!(matches!(
            parse_server_msg(r#"{"t":"hologram","x":1}"#),
            Ok(None)
        ));
        // …but real garbage still errors…
        assert!(parse_server_msg("not json at all").is_err());
        // …and so does a KNOWN tag with broken fields.
        assert!(parse_server_msg(r#"{"t":"update","seq":"NaN","blob":""}"#).is_err());
        // Unknown extra fields inside a known variant stay accepted.
        assert!(matches!(
            parse_server_msg(r#"{"t":"ack","seq":4,"future_field":true}"#),
            Ok(Some(ServerMsg::Ack { seq: 4 }))
        ));
    }

    #[test]
    fn protocol_messages_roundtrip_json() {
        let hello = ClientMsg::Hello {
            vault_id: "ab".repeat(16),
            token: "cd".repeat(32),
            since_seq: 42,
            caps: vec![CAP_PRESENCE.to_string()],
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert!(json.contains("\"t\":\"hello\""));
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        match back {
            ClientMsg::Hello { since_seq, .. } => assert_eq!(since_seq, 42),
            other => panic!("expected hello, got {other:?}"),
        }

        let update = ServerMsg::Update {
            seq: 7,
            blob: vec![0, 1, 2, 255],
        };
        let json = serde_json::to_string(&update).unwrap();
        let back: ServerMsg = serde_json::from_str(&json).unwrap();
        match back {
            ServerMsg::Update { seq, blob } => {
                assert_eq!(seq, 7);
                assert_eq!(blob, vec![0, 1, 2, 255]);
            }
            other => panic!("expected update, got {other:?}"),
        }
    }
}
