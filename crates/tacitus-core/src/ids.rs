use sha2::{Digest, Sha256};
use std::fmt::Write;

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
}
