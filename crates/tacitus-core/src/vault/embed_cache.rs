//! On-disk vector cache (Part I.1), the Rust port of the TS `vault/embed-cache.ts`.
//!
//! Wraps any [`Embedder`] with a persistent cache under `.tacitus/vectors/*.json`
//! so an expensive neural embedder never recomputes a vector for unchanged text —
//! even across restarts. Keyed by content hash (`stable_id(text, "v")`). The
//! deterministic [`HashingEmbedder`] doesn't need this (it's cheap); the cache is
//! the seam that makes a future neural embedder practical.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::ids::stable_id;

use super::embed::Embedder;

type Store = HashMap<String, Vec<f32>>;

pub struct CachedEmbedder<E: Embedder> {
    inner: E,
    cache_file: PathBuf,
    store: Mutex<Option<Store>>,
}

impl<E: Embedder> CachedEmbedder<E> {
    pub fn new(inner: E, cache_file: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            cache_file: cache_file.into(),
            store: Mutex::new(None),
        }
    }

    fn save(&self, store: &Store) -> std::io::Result<()> {
        if let Some(parent) = self.cache_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.cache_file.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string(store).unwrap_or_default())?;
        fs::rename(&tmp, &self.cache_file)?;
        Ok(())
    }
}

fn key_of(text: &str) -> String {
    stable_id(text, "v")
}

impl<E: Embedder> Embedder for CachedEmbedder<E> {
    fn dim(&self) -> usize {
        self.inner.dim()
    }

    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        let mut guard = self.store.lock().expect("embed cache mutex poisoned");
        if guard.is_none() {
            // Lazily load; a missing or corrupt cache file is treated as empty.
            let loaded = fs::read_to_string(&self.cache_file)
                .ok()
                .and_then(|raw| serde_json::from_str::<Store>(&raw).ok())
                .unwrap_or_default();
            *guard = Some(loaded);
        }
        let store = guard.as_mut().expect("store initialized above");

        // Unique cache misses, preserving first-seen order (mirrors the TS Set).
        let mut missing: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for text in texts {
            let key = key_of(text);
            if !store.contains_key(&key) && seen.insert(key) {
                missing.push(text.clone());
            }
        }

        if !missing.is_empty() {
            let vecs = self.inner.embed_batch(&missing);
            for (text, vec) in missing.iter().zip(vecs) {
                store.insert(key_of(text), vec);
            }
            let _ = self.save(store);
        }

        texts
            .iter()
            .map(|t| store.get(&key_of(t)).cloned().unwrap_or_default())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::embed::HashingEmbedder;
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_cache(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-vec-{tag}-{nanos}"));
        dir.join(".tacitus").join("vectors").join("hash.json")
    }

    #[test]
    fn matches_the_inner_embedder_and_writes_a_cache_file() {
        let cache_file = temp_cache("write");
        let cached = CachedEmbedder::new(HashingEmbedder::new(), cache_file.clone());
        let out = cached.embed_batch(&["hello world".to_string()]);
        let direct = HashingEmbedder::new().embed_batch(&["hello world".to_string()]);
        assert_eq!(out, direct);
        assert!(cache_file.exists());
        fs::remove_dir_all(
            cache_file
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap(),
        )
        .ok();
    }

    #[test]
    fn reuses_persisted_vectors_across_instances() {
        let cache_file = temp_cache("persist");
        let v1 = CachedEmbedder::new(HashingEmbedder::new(), cache_file.clone())
            .embed_batch(&["persist me".to_string()]);
        let v2 = CachedEmbedder::new(HashingEmbedder::new(), cache_file.clone())
            .embed_batch(&["persist me".to_string()]);
        assert_eq!(v1, v2);
        fs::remove_dir_all(
            cache_file
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap(),
        )
        .ok();
    }

    /// A spy embedder that counts how many texts it was asked to embed.
    struct Spy {
        embedded: Cell<usize>,
    }
    impl Embedder for Spy {
        fn dim(&self) -> usize {
            4
        }
        fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
            self.embedded.set(self.embedded.get() + texts.len());
            texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect()
        }
    }

    #[test]
    fn only_calls_the_inner_embedder_for_cache_misses() {
        let cache_file = temp_cache("misses");
        let spy = Spy {
            embedded: Cell::new(0),
        };
        let cached = CachedEmbedder::new(spy, cache_file.clone());
        cached.embed_batch(&["a".to_string(), "b".to_string()]);
        cached.embed_batch(&["a".to_string(), "b".to_string(), "c".to_string()]); // only 'c' is a miss
        assert_eq!(cached.inner.embedded.get(), 3);
        fs::remove_dir_all(
            cache_file
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap(),
        )
        .ok();
    }
}
