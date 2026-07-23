use super::index::VaultIndex;
use crate::lexical::{lexical_score, tokenize};
use crate::tokens::estimate;

const SNIPPET_MAX: usize = 240;

#[derive(Default)]
pub struct SearchArgs {
    pub token_budget: Option<usize>,
    pub top_k: Option<usize>,
}

pub struct SearchHit {
    pub note_id: String,
    pub title: String,
    pub score: u32,
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

/// Lexical relevance search returning snippets under a token budget (never whole
/// notes). Semantic/hybrid search is a later Rust milestone.
pub fn search_notes(index: &VaultIndex, query: &str, args: &SearchArgs) -> Vec<SearchHit> {
    let mut ranked: Vec<(&super::types::Note, u32)> = index
        .all()
        .into_iter()
        .map(|n| {
            (
                n,
                lexical_score(query, &format!("{} {}", n.title, n.content)),
            )
        })
        .filter(|(_, score)| *score > 0)
        .collect();
    ranked.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
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
