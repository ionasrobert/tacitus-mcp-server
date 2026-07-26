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
}

// Used by the client drivers (feature "client"); tests exercise it always.
#[cfg_attr(not(feature = "client"), allow(dead_code))]
const KNOWN_SERVER_TAGS: [&str; 5] = ["welcome", "update", "ack", "err", "presence"];

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
            ServerMsg::Welcome { caps, latest_seq } => {
                assert!(caps.is_empty());
                assert_eq!(latest_seq, 9);
            }
            other => panic!("expected welcome, got {other:?}"),
        }
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
