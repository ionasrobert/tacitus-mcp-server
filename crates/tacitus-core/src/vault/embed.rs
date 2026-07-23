//! Embeddings for semantic search (Part I.1/I.3), the Rust port of the TS
//! `vault/embed.ts`.
//!
//! The default [`HashingEmbedder`] is deterministic and offline: it hashes word
//! tokens AND character trigrams into a fixed-dimension vector, so morphological
//! variants ("migrate" ~ "migration") land near each other — something pure
//! lexical matching misses. The fnv1a hashing matches the TS engine bit-for-bit
//! (ASCII-only features), so both engines produce the same vectors. A real
//! neural embedder (candle / an embeddings API) captures synonyms/paraphrases;
//! it drops in behind the same [`Embedder`] trait without touching search.

/// A source of embedding vectors. `embed_batch` takes many texts at once so a
/// batching neural model fits the same interface.
pub trait Embedder {
    fn dim(&self) -> usize;
    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>>;
}

/// Cosine similarity of two unit-normalized vectors (a plain dot product;
/// returns 0 when either side is a zero vector).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut dot = 0f32;
    for i in 0..n {
        dot += a[i] * b[i];
    }
    dot
}

/// 32-bit FNV-1a over the bytes of an ASCII feature string. Matches the TS
/// `fnv1a` (which uses `Math.imul` + `>>> 0`) exactly for ASCII input.
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Feature set for a text: `w:<token>` per word plus `t:<trigram>` over each
/// `#token#`-padded token. Tokens are maximal runs of ASCII `[a-z0-9]` after
/// lowercasing — identical to the TS `/[a-z0-9]+/g` tokenizer, so features (and
/// therefore vectors) match across engines.
fn features(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut feats = Vec::new();
    for token in lower
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|s| !s.is_empty())
    {
        feats.push(format!("w:{token}"));
        let padded = format!("#{token}#");
        // Trigrams: i in 0..=len-3. padded is ASCII, so byte slicing is safe.
        for i in 0..padded.len().saturating_sub(2) {
            feats.push(format!("t:{}", &padded[i..i + 3]));
        }
    }
    feats
}

/// Deterministic, offline embedder: hashes word + trigram features into a
/// fixed-dimension, unit-normalized vector.
pub struct HashingEmbedder {
    dim: usize,
}

impl HashingEmbedder {
    // 2048 dims keep hash collisions low enough that unrelated texts score near
    // zero while morphological variants stay well separated.
    pub fn new() -> Self {
        Self { dim: 2048 }
    }

    pub fn with_dim(dim: usize) -> Self {
        Self { dim }
    }

    /// Embed a single text (used directly by unit tests).
    pub fn embed(&self, text: &str) -> Vec<f32> {
        let mut vec = vec![0f32; self.dim];
        for feature in features(text) {
            let idx = (fnv1a(&feature) % self.dim as u32) as usize;
            vec[idx] += 1.0;
        }
        let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut vec {
                *x /= norm;
            }
        }
        vec
    }
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl Embedder for HashingEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    #[test]
    fn produces_unit_normalized_vectors_of_fixed_dimension() {
        let e = HashingEmbedder::with_dim(256);
        let v = e.embed("hello world");
        assert_eq!(v.len(), 256);
        assert!((norm(&v) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn is_deterministic() {
        let e = HashingEmbedder::with_dim(256);
        assert_eq!(e.embed("same text here"), e.embed("same text here"));
    }

    #[test]
    fn cosine_is_one_for_identical_and_low_for_unrelated() {
        let e = HashingEmbedder::with_dim(256);
        assert!(
            (cosine(
                &e.embed("database migration"),
                &e.embed("database migration")
            ) - 1.0)
                .abs()
                < 1e-4
        );
        assert!(cosine(&e.embed("database migration"), &e.embed("coffee brewing")) < 0.2);
    }

    #[test]
    fn places_morphological_variants_closer_than_unrelated() {
        let e = HashingEmbedder::with_dim(256);
        let q = e.embed("migration");
        let variant = cosine(&q, &e.embed("migrate the schema"));
        let unrelated = cosine(&q, &e.embed("coffee brewing methods"));
        assert!(variant > unrelated);
    }

    #[test]
    fn handles_empty_text_without_nan() {
        let e = HashingEmbedder::with_dim(256);
        let v = e.embed("");
        assert!(v.iter().all(|x| x.is_finite()));
        assert_eq!(cosine(&v, &e.embed("anything")), 0.0);
    }
}
