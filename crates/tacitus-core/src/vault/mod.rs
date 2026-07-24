pub mod embed;
pub mod embed_cache;
pub mod get;
pub mod graph;
pub mod index;
pub mod parse;
pub mod properties;
pub mod rename;
pub mod search;
pub mod tasks;
pub mod template;
pub mod types;
pub mod write;

pub use embed::{cosine, Embedder, HashingEmbedder};
pub use embed_cache::CachedEmbedder;
pub use get::{get_note, GetNoteResult, NoteFormat};
pub use graph::{graph_query, GraphNode, Relation};
pub use index::VaultIndex;
pub use parse::parse_note;
pub use properties::{properties_query, PropFilter, PropOp, PropertiesQueryArgs, PropertiesRow};
pub use rename::rename_note_ops;
pub use search::{search_notes, SearchArgs, SearchHit, SearchMode};
pub use tasks::{list_tasks, note_tasks, toggled_content, Task, TaskFilter};
pub use template::{render_template, template_vars, Template, TemplateStore};
pub use types::{Heading, Note, WikiLink};
pub use write::{
    AuditEntry, ChangeOp, Changeset, CommitResult, DiffEntry, NoteWriter, PermissionScope,
    Proposal, RevertResult,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn make_vault() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!("tacitus-vault-{nanos}"));
        fs::create_dir_all(dir.join("projects")).unwrap();
        fs::write(
            dir.join("index.md"),
            "---\ntitle: Home\n---\n# Home\n\nWelcome. The launch is coming. See [[projects/launch]] and [[ideas]].\n",
        )
        .unwrap();
        fs::write(
            dir.join("projects/launch.md"),
            "---\ntitle: Launch\ntags: [project]\n---\n\nLaunch overview. Related: [[ideas]].\n\n## Timeline\nDates.\n\n## Risks\nMitigations.\n",
        )
        .unwrap();
        fs::write(
            dir.join("ideas.md"),
            "---\ntitle: Ideas\ntags: [idea]\n---\n# Ideas\n\nThe launch deadline is in March. See [[projects/launch]].\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn search_finds_relevant_notes_within_budget() {
        let dir = make_vault();
        let index = VaultIndex::build(&dir).unwrap();
        let embedder = HashingEmbedder::new();
        let hits = search_notes(&index, "launch deadline", &SearchArgs::default(), &embedder);
        assert!(hits.iter().any(|h| h.note_id == "ideas"));
        let scores: Vec<f32> = hits.iter().map(|h| h.score).collect();
        assert_eq!(scores, {
            let mut s = scores.clone();
            s.sort_by(|a, b| b.partial_cmp(a).unwrap());
            s
        });
        for h in &hits {
            assert!(h.snippet.chars().count() <= 240);
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn get_note_outline_and_not_found() {
        let dir = make_vault();
        let index = VaultIndex::build(&dir).unwrap();
        let outline = get_note(&index, "projects/launch", NoteFormat::Outline, None).unwrap();
        assert!(outline.content.contains("Timeline"));
        assert!(outline.content.contains("Risks"));
        assert!(!outline.content.contains("[[ideas]]"));

        let full = get_note(&index, "projects/launch", NoteFormat::Full, Some(3)).unwrap();
        assert!(full.truncated);
        assert!(full.token_count <= 3);

        let err = get_note(&index, "nope", NoteFormat::Full, None).unwrap_err();
        assert_eq!(err.code, "NOTE_NOT_FOUND");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn graph_backlinks_and_neighbors() {
        let dir = make_vault();
        let index = VaultIndex::build(&dir).unwrap();

        let mut backlinks: Vec<String> =
            graph_query(&index, "projects/launch", Relation::Backlinks, 1)
                .unwrap()
                .into_iter()
                .map(|n| n.note_id)
                .collect();
        backlinks.sort();
        assert_eq!(backlinks, vec!["ideas", "index"]);

        let links: Vec<String> = graph_query(&index, "ideas", Relation::Links, 1)
            .unwrap()
            .into_iter()
            .map(|n| n.note_id)
            .collect();
        assert!(links.contains(&"projects/launch".to_string()));

        assert_eq!(
            graph_query(&index, "nope", Relation::Links, 1)
                .unwrap_err()
                .code,
            "NOTE_NOT_FOUND"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
