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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        vault_id: String,
        token: String,
        since_seq: u64,
    },
    Push {
        #[serde(with = "b64")]
        blob: Vec<u8>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        latest_seq: u64,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_messages_roundtrip_json() {
        let hello = ClientMsg::Hello {
            vault_id: "ab".repeat(16),
            token: "cd".repeat(32),
            since_seq: 42,
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
