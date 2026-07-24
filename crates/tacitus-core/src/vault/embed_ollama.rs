//! Neural embeddings through a local Ollama daemon (`/api/embed`) — opt-in
//! via the `ollama` feature and `TACITUS_EMBEDDER=ollama`. Wrap it in
//! [`super::embed_cache::CachedEmbedder`] so vectors persist across runs;
//! probe with [`OllamaEmbedder::probe`] and fall back to the deterministic
//! [`super::embed::HashingEmbedder`] when the daemon isn't running.

use super::embed::Embedder;

pub const DEFAULT_URL: &str = "http://localhost:11434";
pub const DEFAULT_MODEL: &str = "nomic-embed-text";

#[derive(Debug)]
pub struct OllamaEmbedder {
    base_url: String,
    model: String,
    dim: usize,
}

/// Parse an `/api/embed` response body: `{"embeddings": [[f32, …], …]}`.
pub fn parse_embed_response(v: &serde_json::Value) -> Option<Vec<Vec<f32>>> {
    let rows = v.get("embeddings")?.as_array()?;
    rows.iter()
        .map(|row| {
            row.as_array()?
                .iter()
                .map(|x| x.as_f64().map(|f| f as f32))
                .collect()
        })
        .collect()
}

impl OllamaEmbedder {
    /// Probe the daemon with a one-word embed; returns the embedder with the
    /// model's real dimensionality, or an error message for the fallback log.
    pub fn probe(base_url: &str, model: &str) -> Result<Self, String> {
        let rows = request(base_url, model, &["probe".to_string()])?;
        let dim = rows
            .first()
            .map(Vec::len)
            .filter(|d| *d > 0)
            .ok_or_else(|| {
                format!(
                    "model {model:?} returned no embedding — is it pulled? (ollama pull {model})"
                )
            })?;
        Ok(Self {
            base_url: base_url.to_string(),
            model: model.to_string(),
            dim,
        })
    }
}

fn request(base_url: &str, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let body = serde_json::json!({ "model": model, "input": texts });
    let mut response = ureq::post(format!("{base_url}/api/embed"))
        .send_json(&body)
        .map_err(|e| format!("Ollama unreachable at {base_url}: {e}"))?;
    let value: serde_json::Value = response
        .body_mut()
        .read_json()
        .map_err(|e| format!("Ollama /api/embed returned malformed JSON: {e}"))?;
    parse_embed_response(&value)
        .ok_or_else(|| "Ollama /api/embed response missing embeddings".to_string())
}

impl Embedder for OllamaEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    /// Errors degrade to zero vectors (cosine 0 — the hit just doesn't get a
    /// semantic boost); retrieval must never fail because a daemon hiccuped.
    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        if texts.is_empty() {
            return vec![];
        }
        match request(&self.base_url, &self.model, texts) {
            Ok(rows) if rows.len() == texts.len() => rows,
            _ => texts.iter().map(|_| vec![0.0; self.dim]).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_embed_response_extracts_rows() {
        let v = serde_json::json!({ "embeddings": [[0.1, 0.2], [0.3, 0.4]] });
        let rows = parse_embed_response(&v).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 2);
        assert!((rows[1][1] - 0.4).abs() < 1e-6);
    }

    #[test]
    fn parse_embed_response_rejects_malformed_payloads() {
        assert!(parse_embed_response(&serde_json::json!({})).is_none());
        assert!(parse_embed_response(&serde_json::json!({ "embeddings": "nope" })).is_none());
        assert!(parse_embed_response(&serde_json::json!({ "embeddings": [["x"]] })).is_none());
    }

    #[test]
    fn probe_fails_cleanly_when_no_daemon() {
        // A port nothing listens on: the error is a message, not a panic.
        let err = OllamaEmbedder::probe("http://127.0.0.1:9", "any").unwrap_err();
        assert!(err.contains("unreachable"));
    }
}
