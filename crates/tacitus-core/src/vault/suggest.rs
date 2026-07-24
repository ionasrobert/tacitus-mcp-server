use std::collections::HashSet;

use super::embed::{cosine, Embedder};
use super::index::VaultIndex;
use super::search::make_snippet;
use super::types::Note;
use crate::error::TacitusError;
use crate::lexical::tokenize;
use crate::tokens::estimate;

const DEFAULT_TOP_K: usize = 5;
/// Below this blended score a candidate is noise, not a suggestion.
const DEFAULT_MIN_SCORE: f32 = 0.15;
/// Title-mention is the classic auto-link signal — it dominates the blend:
/// a mention alone (0.45) outranks any realistic pure-semantic score (≤0.30).
const MENTION_WEIGHT: f32 = 0.45;
const SEM_WEIGHT: f32 = 0.30;
const TAG_WEIGHT: f32 = 0.15;
/// A note that already links here nudges toward making the link bidirectional.
const BACKLINK_WEIGHT: f32 = 0.10;
/// Cosine below this doesn't count as a "semantic" reason (mirrors search's SEMANTIC_MIN_SIM).
const SEMANTIC_REASON_MIN: f32 = 0.15;
/// Shared tags beyond this add no more signal.
const TAG_CAP: usize = 3;

#[derive(Default)]
pub struct SuggestArgs {
    pub top_k: Option<usize>,
    pub min_score: Option<f32>,
    pub token_budget: Option<usize>,
}

#[derive(Debug)]
pub struct LinkSuggestion {
    pub note_id: String,
    pub title: String,
    pub score: f32,
    /// Machine-readable signals that fired: "title_mentioned", "semantic",
    /// "shared_tags", "backlink" — so an agent can decide, not guess.
    pub reasons: Vec<String>,
    pub snippet: String,
    pub token_count: usize,
}

/// True when the candidate's title (or id basename) appears as a contiguous
/// token window in the source content — token matching, so "Art" never fires
/// inside "start" and case/punctuation don't matter.
fn mentions(source_tokens: &[String], title: &str, basename: &str) -> bool {
    for needle in [tokenize(title), tokenize(basename)] {
        if needle.is_empty() || needle.iter().map(|t| t.len()).sum::<usize>() < 3 {
            continue;
        }
        if source_tokens
            .windows(needle.len())
            .any(|w| w == needle.as_slice())
        {
            return true;
        }
    }
    false
}

/// Suggest wikilinks a note is missing: rank unlinked candidates by title
/// mentions in the source, embedding similarity, shared tags, and existing
/// backlinks. Pure over the index (no I/O); output bounded like search.
pub fn suggest_links(
    index: &VaultIndex,
    note_id: &str,
    embedder: &dyn Embedder,
    args: &SuggestArgs,
) -> Result<Vec<LinkSuggestion>, TacitusError> {
    let Some(source) = index.get(note_id) else {
        return Err(TacitusError::new(
            "NOTE_NOT_FOUND",
            format!("No note with id \"{note_id}\"."),
            "Use list_notes to discover valid note ids.",
        ));
    };

    let linked: HashSet<&str> = index
        .outgoing(note_id)
        .iter()
        .map(|n| n.id.as_str())
        .collect();
    let backlinkers: HashSet<String> = index
        .backlinks(note_id)
        .iter()
        .map(|n| n.id.clone())
        .collect();

    let candidates: Vec<&Note> = index
        .all()
        .into_iter()
        .filter(|n| n.id != note_id && !linked.contains(n.id.as_str()))
        .collect();

    let mut texts = Vec::with_capacity(candidates.len() + 1);
    texts.push(format!("{} {}", source.title, source.content));
    for note in &candidates {
        texts.push(format!("{} {}", note.title, note.content));
    }
    let vecs = embedder.embed_batch(&texts);
    let source_vec = vecs.first().cloned().unwrap_or_default();

    let source_tokens = tokenize(&source.content);
    let source_tags: HashSet<&str> = source.tags.iter().map(|s| s.as_str()).collect();
    let min_score = args.min_score.unwrap_or(DEFAULT_MIN_SCORE);

    let mut scored: Vec<(&Note, f32, Vec<String>, bool)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, note)| {
            let sem = vecs
                .get(i + 1)
                .map(|v| cosine(&source_vec, v).max(0.0))
                .unwrap_or(0.0);
            let basename = note.id.rsplit('/').next().unwrap_or(&note.id);
            let mentioned = mentions(&source_tokens, &note.title, basename);
            let shared_tags = note
                .tags
                .iter()
                .filter(|t| source_tags.contains(t.as_str()))
                .count();
            let backlink = backlinkers.contains(&note.id);

            let score = if mentioned { MENTION_WEIGHT } else { 0.0 }
                + SEM_WEIGHT * sem
                + TAG_WEIGHT * (shared_tags.min(TAG_CAP) as f32 / TAG_CAP as f32)
                + if backlink { BACKLINK_WEIGHT } else { 0.0 };
            if score < min_score {
                return None;
            }

            let mut reasons = Vec::new();
            if mentioned {
                reasons.push("title_mentioned".to_string());
            }
            if sem >= SEMANTIC_REASON_MIN {
                reasons.push("semantic".to_string());
            }
            if shared_tags > 0 {
                reasons.push("shared_tags".to_string());
            }
            if backlink {
                reasons.push("backlink".to_string());
            }
            Some((*note, score, reasons, mentioned))
        })
        .collect();

    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.id.cmp(&b.0.id))
    });
    scored.truncate(args.top_k.unwrap_or(DEFAULT_TOP_K));

    let budget = args.token_budget.unwrap_or(usize::MAX);
    let mut suggestions = Vec::new();
    let mut used = 0usize;
    for (note, score, reasons, mentioned) in scored {
        // A mention's snippet shows WHERE in the source the link would go;
        // otherwise show what the candidate is about.
        let snippet = if mentioned {
            make_snippet(&note.title, &source.content)
        } else {
            make_snippet(&source.title, &note.content)
        };
        let token_count = estimate(&snippet);
        if used + token_count > budget {
            continue;
        }
        suggestions.push(LinkSuggestion {
            note_id: note.id.clone(),
            title: note.title.clone(),
            score,
            reasons,
            snippet,
            token_count,
        });
        used += token_count;
    }
    Ok(suggestions)
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
        dir.push(format!("tacitus-sugg-{nanos}"));
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

    fn ids(suggestions: &[LinkSuggestion]) -> Vec<String> {
        suggestions.iter().map(|s| s.note_id.clone()).collect()
    }

    #[test]
    fn title_mention_outranks_pure_semantic() {
        let (dir, index) = vault(&[
            (
                "daily",
                "Debugged the Kubernetes cluster today; the autoscaler kept evicting pods during deploys.",
            ),
            (
                "kubernetes",
                "# Kubernetes\n\nCluster operations: autoscaling, pod eviction, deploy runbooks.",
            ),
            (
                "containers",
                "# Containers\n\nCluster autoscaling and pod eviction during deploys, runbooks.",
            ),
        ]);
        let embedder = HashingEmbedder::new();
        let res = suggest_links(&index, "daily", &embedder, &SuggestArgs::default()).unwrap();
        assert_eq!(res.first().map(|s| s.note_id.as_str()), Some("kubernetes"));
        assert!(res[0].reasons.contains(&"title_mentioned".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn excludes_self_and_already_linked_notes() {
        let (dir, index) = vault(&[
            (
                "daily",
                "Worked on [[kubernetes]] today. Also drafted the Docker migration checklist for the cluster.",
            ),
            ("kubernetes", "# Kubernetes\n\nCluster ops."),
            ("docker", "# Docker\n\nContainer images and the migration checklist."),
        ]);
        let embedder = HashingEmbedder::new();
        let res = suggest_links(&index, "daily", &embedder, &SuggestArgs::default()).unwrap();
        let got = ids(&res);
        assert!(!got.contains(&"daily".to_string()));
        assert!(!got.contains(&"kubernetes".to_string()));
        assert!(got.contains(&"docker".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backlinking_note_remains_a_candidate_with_backlink_reason() {
        let (dir, index) = vault(&[
            (
                "project",
                "Launch plan for the search feature: indexing, ranking, and rollout.",
            ),
            (
                "retro",
                "Retro notes on the launch plan: indexing and ranking learnings. See [[project]].",
            ),
        ]);
        let embedder = HashingEmbedder::new();
        let res = suggest_links(&index, "project", &embedder, &SuggestArgs::default()).unwrap();
        let retro = res
            .iter()
            .find(|s| s.note_id == "retro")
            .expect("backlinking note should stay a candidate");
        assert!(retro.reasons.contains(&"backlink".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn returns_note_not_found_for_missing_source() {
        let (dir, index) = vault(&[("a", "Something.")]);
        let embedder = HashingEmbedder::new();
        let err = suggest_links(&index, "nope", &embedder, &SuggestArgs::default()).unwrap_err();
        assert_eq!(err.code, "NOTE_NOT_FOUND");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn respects_token_budget_hard_cap() {
        let (dir, index) = vault(&[
            ("hub", "Alpha ideas, Beta drafts, Gamma plans."),
            ("alpha", "# Alpha\n\nIdeas for the alpha track."),
            ("beta", "# Beta\n\nDrafts for the beta track."),
            ("gamma", "# Gamma\n\nPlans for the gamma track."),
        ]);
        let embedder = HashingEmbedder::new();
        let res = suggest_links(
            &index,
            "hub",
            &embedder,
            &SuggestArgs {
                token_budget: Some(15),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!res.is_empty());
        assert!(res.len() < 3, "budget should cut some suggestions");
        assert!(res.iter().map(|s| s.token_count).sum::<usize>() <= 15);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn min_score_and_top_k_bound_the_output() {
        let (dir, index) = vault(&[
            ("daily", "Reviewed the Alpha spec and the Beta rollout."),
            ("alpha", "# Alpha\n\nSpec details."),
            ("beta", "# Beta\n\nRollout details."),
            (
                "gardening",
                "# Gardening\n\nPrune roses in spring; water tomatoes at dawn.",
            ),
        ]);
        let embedder = HashingEmbedder::new();
        let defaults = suggest_links(&index, "daily", &embedder, &SuggestArgs::default()).unwrap();
        assert!(!ids(&defaults).contains(&"gardening".to_string()));
        let one = suggest_links(
            &index,
            "daily",
            &embedder,
            &SuggestArgs {
                top_k: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(one.len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shared_tags_surface_as_a_reason() {
        let (dir, index) = vault(&[
            (
                "a",
                "---\ntitle: Sprint planning\ntags: [project, q3]\n---\n\nPlanning the sprint backlog and estimates.",
            ),
            (
                "b",
                "---\ntitle: Sprint retro\ntags: [project, q3]\n---\n\nRetro on the sprint backlog and estimates.",
            ),
        ]);
        let embedder = HashingEmbedder::new();
        let res = suggest_links(&index, "a", &embedder, &SuggestArgs::default()).unwrap();
        let b = res
            .iter()
            .find(|s| s.note_id == "b")
            .expect("tag-sharing note should be suggested");
        assert!(b.reasons.contains(&"shared_tags".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mention_snippet_shows_source_context() {
        let (dir, index) = vault(&[
            (
                "journal",
                "Morning standup, then code review for the sync branch. The Roadmap review went well after lunch, we cut two items.",
            ),
            ("roadmap", "# Roadmap\n\nQuarters and milestones."),
        ]);
        let embedder = HashingEmbedder::new();
        let res = suggest_links(&index, "journal", &embedder, &SuggestArgs::default()).unwrap();
        let road = res
            .iter()
            .find(|s| s.note_id == "roadmap")
            .expect("mentioned note should be suggested");
        assert!(road.reasons.contains(&"title_mentioned".to_string()));
        assert!(
            road.snippet.contains("went well"),
            "snippet shows where the link would go in the source"
        );
        assert!(
            !road.snippet.contains("milestones"),
            "snippet is source context, not candidate content"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
