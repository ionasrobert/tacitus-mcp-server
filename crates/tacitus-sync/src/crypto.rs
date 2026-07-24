//! End-to-end encryption: the relay only ever sees ciphertext.
//!
//! The secret is a generated **vault code** (`tacitus-xxxx-…`, ~160 bits) —
//! not a human passphrase, so weak-passphrase collisions and salt
//! distribution are non-problems. Everything derives deterministically from
//! the code: the same code on any device is the same vault.
//!
//!   root       = argon2id(code, fixed salt v1)
//!   vault_key  = HKDF(root, "tacitus/v1/vault-key")     — AEAD key
//!   vault_id   = HKDF(root, "tacitus/v1/vault-id")      — relay-visible
//!   auth_token = HKDF(root, "tacitus/v1/relay-token")   — one-way from root
//!
//! AEAD is XChaCha20-Poly1305 with a random 24-byte nonce per update and the
//! vault id as AAD. Note ids and device ids live INSIDE the ciphertext.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload as AeadPayload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::protocol::b64;
use crate::SyncError;

const BASE32_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const CODE_BYTES: usize = 20; // 160 bits → 32 base32 chars
const NONCE_LEN: usize = 24;

/// The shareable vault secret: `tacitus-xxxx-xxxx-…` (8 groups of 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultCode(String);

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for byte in bytes {
        acc = (acc << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32_ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32_ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

impl VaultCode {
    pub fn generate() -> Self {
        let mut bytes = [0u8; CODE_BYTES];
        OsRng.fill_bytes(&mut bytes);
        let raw = base32_encode(&bytes);
        let grouped: Vec<&str> = raw
            .as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).expect("base32 is ascii"))
            .collect();
        VaultCode(format!("tacitus-{}", grouped.join("-")))
    }

    /// Accepts the code with or without group dashes; normalizes case.
    pub fn parse(s: &str) -> Result<Self, SyncError> {
        let lower = s.trim().to_lowercase();
        let rest = lower.strip_prefix("tacitus-").ok_or_else(|| SyncError {
            code: "INVALID_CODE",
            reason: "A vault code starts with \"tacitus-\".".into(),
        })?;
        let compact: String = rest.chars().filter(|c| *c != '-').collect();
        let expected_len = CODE_BYTES * 8 / 5 + usize::from(!(CODE_BYTES * 8).is_multiple_of(5));
        if compact.len() != expected_len || !compact.bytes().all(|b| BASE32_ALPHABET.contains(&b)) {
            return Err(SyncError {
                code: "INVALID_CODE",
                reason: "Malformed vault code — copy it exactly as generated.".into(),
            });
        }
        let grouped: Vec<&str> = compact
            .as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).expect("base32 is ascii"))
            .collect();
        Ok(VaultCode(format!("tacitus-{}", grouped.join("-"))))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub struct Keys {
    pub vault_key: [u8; 32],
    pub vault_id: String,
    pub auth_token: String,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Deterministic: same code → same keys on every device.
pub fn derive_keys(code: &VaultCode) -> Keys {
    let salt_full = Sha256::digest(b"tacitus.md/sync/salt/v1");
    let params = Params::new(64 * 1024, 3, 1, Some(32)).expect("static argon2 params");
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut root = [0u8; 32];
    argon
        .hash_password_into(code.as_str().as_bytes(), &salt_full[..16], &mut root)
        .expect("argon2 with static params cannot fail");

    let hk = Hkdf::<Sha256>::new(None, &root);
    let mut vault_key = [0u8; 32];
    hk.expand(b"tacitus/v1/vault-key", &mut vault_key)
        .expect("static hkdf length");
    let mut id = [0u8; 16];
    hk.expand(b"tacitus/v1/vault-id", &mut id)
        .expect("static hkdf length");
    let mut token = [0u8; 32];
    hk.expand(b"tacitus/v1/relay-token", &mut token)
        .expect("static hkdf length");
    Keys {
        vault_key,
        vault_id: hex(&id),
        auth_token: hex(&token),
    }
}

/// One CRDT update destined for a doc (`n:…`, `m:…`, or `manifest`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocUpdate {
    pub doc: String,
    #[serde(with = "b64")]
    pub u: Vec<u8>,
}

/// What actually travels (inside the ciphertext).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPayload {
    pub v: u32,
    pub device: String,
    pub updates: Vec<DocUpdate>,
}

/// Encrypt a payload: `nonce(24) || ciphertext`, AAD = vault_id.
pub fn seal(key: &[u8; 32], vault_id: &str, payload: &SyncPayload) -> Result<Vec<u8>, SyncError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let msg = serde_json::to_vec(payload).map_err(|e| SyncError {
        code: "INTERNAL",
        reason: format!("payload serialization failed: {e}"),
    })?;
    let ct = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            AeadPayload {
                msg: &msg,
                aad: vault_id.as_bytes(),
            },
        )
        .map_err(|_| SyncError {
            code: "CRYPTO",
            reason: "encryption failed".into(),
        })?;
    let mut blob = Vec::with_capacity(NONCE_LEN + ct.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ct);
    Ok(blob)
}

/// Decrypt and authenticate a blob; rejects tampering and wrong keys.
pub fn open(key: &[u8; 32], vault_id: &str, blob: &[u8]) -> Result<SyncPayload, SyncError> {
    if blob.len() <= NONCE_LEN {
        return Err(SyncError {
            code: "CRYPTO",
            reason: "blob too short".into(),
        });
    }
    let cipher = XChaCha20Poly1305::new(key.into());
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    let msg = cipher
        .decrypt(
            XNonce::from_slice(nonce),
            AeadPayload {
                msg: ct,
                aad: vault_id.as_bytes(),
            },
        )
        .map_err(|_| SyncError {
            code: "CRYPTO",
            reason: "decryption failed — wrong vault code or corrupted data".into(),
        })?;
    serde_json::from_slice(&msg).map_err(|e| SyncError {
        code: "CRYPTO",
        reason: format!("payload deserialization failed: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> SyncPayload {
        SyncPayload {
            v: 1,
            device: "dev_test".into(),
            updates: vec![DocUpdate {
                doc: "n:projects/launch".into(),
                u: vec![1, 2, 3, 4],
            }],
        }
    }

    #[test]
    fn vault_code_derivation_is_deterministic_across_devices() {
        let code = VaultCode::generate();
        let reparsed = VaultCode::parse(code.as_str()).unwrap();
        assert_eq!(code, reparsed);

        let k1 = derive_keys(&code);
        let k2 = derive_keys(&reparsed);
        assert_eq!(k1.vault_key, k2.vault_key);
        assert_eq!(k1.vault_id, k2.vault_id);
        assert_eq!(k1.auth_token, k2.auth_token);
        assert_eq!(k1.vault_id.len(), 32);
    }

    #[test]
    fn distinct_codes_yield_distinct_vault_ids_and_keys() {
        let a = derive_keys(&VaultCode::generate());
        let b = derive_keys(&VaultCode::generate());
        assert_ne!(a.vault_id, b.vault_id);
        assert_ne!(a.vault_key, b.vault_key);
        assert_ne!(a.auth_token, b.auth_token);
    }

    #[test]
    fn parse_rejects_garbage_and_accepts_dashless() {
        assert!(VaultCode::parse("not-a-code").is_err());
        assert!(VaultCode::parse("tacitus-short").is_err());
        assert!(VaultCode::parse("tacitus-ABCD-!!!!-abcd-abcd-abcd-abcd-abcd-abcd").is_err());

        let code = VaultCode::generate();
        let dashless: String = code
            .as_str()
            .strip_prefix("tacitus-")
            .unwrap()
            .chars()
            .filter(|c| *c != '-')
            .collect();
        let reparsed = VaultCode::parse(&format!("tacitus-{dashless}")).unwrap();
        assert_eq!(code, reparsed);
    }

    #[test]
    fn seal_open_roundtrips() {
        let keys = derive_keys(&VaultCode::generate());
        let blob = seal(&keys.vault_key, &keys.vault_id, &payload()).unwrap();
        let opened = open(&keys.vault_key, &keys.vault_id, &blob).unwrap();
        assert_eq!(opened.device, "dev_test");
        assert_eq!(opened.updates[0].doc, "n:projects/launch");
        assert_eq!(opened.updates[0].u, vec![1, 2, 3, 4]);
    }

    #[test]
    fn open_rejects_tampered_ciphertext() {
        let keys = derive_keys(&VaultCode::generate());
        let mut blob = seal(&keys.vault_key, &keys.vault_id, &payload()).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(open(&keys.vault_key, &keys.vault_id, &blob).is_err());
    }

    #[test]
    fn open_rejects_wrong_key() {
        let keys = derive_keys(&VaultCode::generate());
        let other = derive_keys(&VaultCode::generate());
        let blob = seal(&keys.vault_key, &keys.vault_id, &payload()).unwrap();
        assert!(open(&other.vault_key, &keys.vault_id, &blob).is_err());
    }
}
