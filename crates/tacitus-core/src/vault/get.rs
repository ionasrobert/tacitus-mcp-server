use super::index::VaultIndex;
use crate::error::TacitusError;
use crate::tokens::estimate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteFormat {
    Outline,
    FrontmatterOnly,
    Full,
}

#[derive(Debug, Clone)]
pub struct GetNoteResult {
    pub note_id: String,
    pub title: String,
    pub format: NoteFormat,
    pub content: String,
    pub token_count: usize,
    pub truncated: bool,
}

/// Progressive disclosure: outline (headings) → frontmatter → full, with an
/// optional max_tokens ceiling so a single note can't blow the context window.
pub fn get_note(
    index: &VaultIndex,
    id: &str,
    format: NoteFormat,
    max_tokens: Option<usize>,
) -> Result<GetNoteResult, TacitusError> {
    let note = index.get(id).ok_or_else(|| {
        TacitusError::new(
            "NOTE_NOT_FOUND",
            format!("No note with id \"{id}\"."),
            "Use search or list_notes to discover valid note ids.",
        )
    })?;

    let mut content = match format {
        NoteFormat::Outline => note
            .headings
            .iter()
            .map(|h| format!("{}- {}", "  ".repeat(h.level - 1), h.text))
            .collect::<Vec<_>>()
            .join("\n"),
        NoteFormat::FrontmatterOnly => serde_yaml::to_string(&note.frontmatter)
            .unwrap_or_default()
            .trim_end()
            .to_string(),
        NoteFormat::Full => note.content.clone(),
    };

    let mut truncated = false;
    if let Some(max) = max_tokens {
        let max_chars = max * 4;
        if content.chars().count() > max_chars {
            let cut: String = content.chars().take(max_chars).collect();
            content = match cut.rfind(' ') {
                Some(sp) if sp > 0 => cut[..sp].trim_end().to_string(),
                _ => cut.trim_end().to_string(),
            };
            truncated = true;
        }
    }

    Ok(GetNoteResult {
        note_id: note.id.clone(),
        title: note.title.clone(),
        format,
        token_count: estimate(&content),
        content,
        truncated,
    })
}
