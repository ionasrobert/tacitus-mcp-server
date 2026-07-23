use std::collections::HashMap;

use super::embed::{cosine, Embedder};
use super::index::VaultIndex;
use super::types::Note;
use crate::lexical::{lexical_score, tokenize};
use crate::tokens::estimate;

const SNIPPET_MAX: usize = 240;
/// Minimum semantic similarity for a note with no lexical match to be recalled.
const SEMANTIC_MIN_SIM: f32 = 0.15;

/// Search strategy: lexical (exact terms), semantic (embedding cosine), or
/// hybrid (blend — the default: lexical precision plus semantic recall).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SearchMode {
    Lexical,
    Semantic,
    #[default]
    Hybrid,
}

#[derive(Default)]
pub struct SearchArgs {
    pub mode: SearchMode,
    pub token_budget: Option<usize>,
    pub top_k: Option<usize>,
}

pub struct SearchHit {
    pub note_id: String,
    pub title: String,
    pub score: f32,
    pub snippet: String,
    pub token_count: usize,
}

fn make_snippet(query: &str, content: &str) -> String {
    let terms: std::collections::HashSet<String> = tokenize(query).into_iter().collect();
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = collapsed.to_lowercase();
    let at = terms
        .iter()
        .filter_map(|t| lower.find(t.as_str()))
        .min()
        .unwrap_or(0);
    let start = at.saturating_sub(40);
    collapsed.chars().skip(start).take(SNIPPET_MAX).collect()
}

/// Rank notes by relevance and return snippets under a token budget (Part I.1),
/// never whole notes. `mode` selects lexical, semantic, or (default) hybrid
/// scoring; the caller supplies the embedder (a [`super::embed::HashingEmbedder`]
/// by default, a cached/neural one when configured).
pub fn search_notes(
    index: &VaultIndex,
    query: &str,
    args: &SearchArgs,
    embedder: &dyn Embedder,
) -> Vec<SearchHit> {
    let notes = index.all();

    let mut lexical: HashMap<&str, u32> = HashMap::new();
    let mut max_lexical = 0u32;
    for note in &notes {
        let score = lexical_score(query, &format!("{} {}", note.title, note.content));
        lexical.insert(note.id.as_str(), score);
        max_lexical = max_lexical.max(score);
    }

    let mut semantic: HashMap<&str, f32> = HashMap::new();
    if args.mode != SearchMode::Lexical {
        let mut texts = Vec::with_capacity(notes.len() + 1);
        texts.push(query.to_string());
        for note in &notes {
            texts.push(format!("{} {}", note.title, note.content));
        }
        let vecs = embedder.embed_batch(&texts);
        let query_vec = vecs.first().cloned().unwrap_or_default();
        for (i, note) in notes.iter().enumerate() {
            let sim = vecs
                .get(i + 1)
                .map(|v| cosine(&query_vec, v).max(0.0))
                .unwrap_or(0.0);
            semantic.insert(note.id.as_str(), sim);
        }
    }

    let mut ranked: Vec<(&Note, f32)> = notes
        .iter()
        .filter_map(|note| {
            let lex = *lexical.get(note.id.as_str()).unwrap_or(&0);
            let sem = *semantic.get(note.id.as_str()).unwrap_or(&0.0);
            let (score, include) = match args.mode {
                SearchMode::Lexical => (lex as f32, lex > 0),
                SearchMode::Semantic => (sem, sem >= SEMANTIC_MIN_SIM),
                SearchMode::Hybrid => {
                    let lex_norm = if max_lexical > 0 {
                        lex as f32 / max_lexical as f32
                    } else {
                        0.0
                    };
                    (
                        0.5 * lex_norm + 0.5 * sem,
                        lex > 0 || sem >= SEMANTIC_MIN_SIM,
                    )
                }
            };
            if include {
                Some((*note, score))
            } else {
                None
            }
        })
        .collect();

    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if let Some(k) = args.top_k {
        ranked.truncate(k);
    }

    let budget = args.token_budget.unwrap_or(usize::MAX);
    let mut hits = Vec::new();
    let mut used = 0usize;
    for (note, score) in ranked {
        let snippet = make_snippet(query, &note.content);
        let token_count = estimate(&snippet);
        if used + token_count > budget {
            continue;
        }
        hits.push(SearchHit {
            note_id: note.id.clone(),
            title: note.title.clone(),
            score,
            snippet,
            token_count,
        });
        used += token_count;
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::super::embed::HashingEmbedder;
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn vault(notes: &[(&str, &str)]) -> (PathBuf, VaultIndex) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-sem-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        for (id, content) in notes {
            let path = dir.join(format!("{id}.md"));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
        let index = VaultIndex::build(&dir).unwrap();
        (dir, index)
    }

    fn ids(hits: &[SearchHit]) -> Vec<String> {
        hits.iter().map(|h| h.note_id.clone()).collect()
    }

    #[test]
    fn hybrid_surfaces_a_morphological_variant_that_lexical_misses() {
        let (dir, index) = vault(&[
            ("a", "Database migration strategy and rollout."),
            ("b", "How to migrate the schema safely."),
            ("c", "Gardening tips for spring."),
        ]);
        let embedder = HashingEmbedder::new();
        let lexical = search_notes(
            &index,
            "migration",
            &SearchArgs {
                mode: SearchMode::Lexical,
                ..Default::default()
            },
            &embedder,
        );
        let hybrid = search_notes(
            &index,
            "migration",
            &SearchArgs {
                mode: SearchMode::Hybrid,
                ..Default::default()
            },
            &embedder,
        );
        assert!(!ids(&lexical).contains(&"b".to_string()));
        assert!(ids(&hybrid).contains(&"b".to_string()));
        assert!(!ids(&hybrid).contains(&"c".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn semantic_mode_ranks_by_similarity_and_respects_budget() {
        let (dir, index) = vault(&[
            ("a", "kubernetes cluster autoscaling and pods"),
            ("b", "unrelated poetry about the calm sea"),
        ]);
        let embedder = HashingEmbedder::new();
        let res = search_notes(
            &index,
            "kubernetes scaling",
            &SearchArgs {
                mode: SearchMode::Semantic,
                token_budget: Some(50),
                top_k: None,
            },
            &embedder,
        );
        assert_eq!(res.first().map(|h| h.note_id.as_str()), Some("a"));
        assert!(res.iter().map(|h| h.token_count).sum::<usize>() <= 50);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn defaults_to_hybrid_mode() {
        let (dir, index) = vault(&[("a", "launch deadline in March")]);
        let embedder = HashingEmbedder::new();
        let def = search_notes(&index, "launch", &SearchArgs::default(), &embedder);
        let hybrid = search_notes(
            &index,
            "launch",
            &SearchArgs {
                mode: SearchMode::Hybrid,
                ..Default::default()
            },
            &embedder,
        );
        assert_eq!(ids(&def), ids(&hybrid));
        fs::remove_dir_all(&dir).ok();
    }
}
