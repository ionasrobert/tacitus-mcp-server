//! Tasks as typed entities (the "Tasks/Kanban" capability, agent-native):
//! checklist lines (`- [ ] text`) anywhere in the vault are parsed into
//! queryable entities — status, due date (`due:YYYY-MM-DD` or `📅 YYYY-MM-DD`),
//! inline #tags — instead of agents regexing over raw Markdown.
//!
//! Toggling is a PURE function ([`toggled_content`]): callers snapshot the
//! note's content, compute the new content, then commit it through the
//! transactional `NoteWriter` (versioned + audited) — so no lock is ever held
//! across a write. The expected task text acts as an optimistic-concurrency
//! guard: if the line changed since the caller read it, the result is a
//! structured CONFLICT, never a flip of the wrong task.

use std::sync::OnceLock;

use regex::Regex;

use super::index::VaultIndex;
use crate::error::TacitusError;
use crate::tokens::estimate;

#[derive(Clone, Debug)]
pub struct Task {
    pub note_id: String,
    /// 0-based line index within the note's content (frontmatter excluded).
    pub line: usize,
    /// The text after the checkbox, metadata included.
    pub text: String,
    pub done: bool,
    /// ISO date parsed from `due:YYYY-MM-DD` or `📅 YYYY-MM-DD`, if present.
    pub due: Option<String>,
    pub tags: Vec<String>,
    pub token_count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    /// Some(false) = open, Some(true) = done, None = all.
    pub done: Option<bool>,
    /// Only tasks with a due date strictly before this ISO date.
    pub due_before: Option<String>,
    /// Only tasks with a due date on/after this ISO date.
    pub due_after: Option<String>,
    pub tag: Option<String>,
    pub note_id: Option<String>,
    /// Max tasks (default 100).
    pub limit: Option<usize>,
    /// Hard token ceiling across returned tasks.
    pub token_budget: Option<usize>,
}

fn task_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*[-*] \[([ xX])\] (.*)$").unwrap())
}

fn due_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:due:|\x{1F4C5}\s*)(\d{4}-\d{2}-\d{2})").unwrap())
}

fn tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|\s)#([A-Za-z0-9_/-]+)").unwrap())
}

/// Parse one content line as a task; None if it isn't a checklist line.
fn parse_task_line(note_id: &str, line_idx: usize, line: &str) -> Option<Task> {
    let cap = task_re().captures(line)?;
    let done = !cap.get(1).is_some_and(|m| m.as_str() == " ");
    let text = cap.get(2).map_or("", |m| m.as_str()).trim_end().to_string();
    let due = due_re()
        .captures(&text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string());
    let tags = tag_re()
        .captures_iter(&text)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect();
    let token_count = estimate(&text) + 8; // text + structural overhead
    Some(Task {
        note_id: note_id.to_string(),
        line: line_idx,
        text,
        done,
        due,
        tags,
        token_count,
    })
}

/// All tasks in a note's content, in line order.
pub fn note_tasks(note_id: &str, content: &str) -> Vec<Task> {
    content
        .lines()
        .enumerate()
        .filter_map(|(i, line)| parse_task_line(note_id, i, line))
        .collect()
}

const DEFAULT_LIMIT: usize = 100;

/// Query the vault's tasks. Sorted by due date (missing last), then note_id,
/// then line; capped by `limit` and `token_budget`.
pub fn list_tasks(index: &VaultIndex, filter: &TaskFilter) -> Vec<Task> {
    let mut tasks: Vec<Task> = index
        .all()
        .into_iter()
        .filter(|n| filter.note_id.as_deref().is_none_or(|id| n.id == id))
        .flat_map(|n| note_tasks(&n.id, &n.content))
        .filter(|t| filter.done.is_none_or(|d| t.done == d))
        .filter(|t| {
            filter
                .due_before
                .as_deref()
                .is_none_or(|cut| t.due.as_deref().is_some_and(|due| due < cut))
        })
        .filter(|t| {
            filter
                .due_after
                .as_deref()
                .is_none_or(|cut| t.due.as_deref().is_some_and(|due| due >= cut))
        })
        .filter(|t| {
            filter
                .tag
                .as_deref()
                .is_none_or(|tag| t.tags.iter().any(|x| x == tag))
        })
        .collect();

    tasks.sort_by(|a, b| {
        match (&a.due, &b.due) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less, // missing due sorts last
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.note_id.cmp(&b.note_id))
        .then_with(|| a.line.cmp(&b.line))
    });
    tasks.truncate(filter.limit.unwrap_or(DEFAULT_LIMIT));

    let budget = filter.token_budget.unwrap_or(usize::MAX);
    let mut used = 0usize;
    tasks.retain(|t| {
        if used + t.token_count > budget {
            false
        } else {
            used += t.token_count;
            true
        }
    });
    tasks
}

/// Pure toggle: validate the task at `line` in `content` and return the new
/// content with its checkbox flipped. `expect_text` is the task text the
/// caller last saw — if the line moved or changed, this is a CONFLICT
/// (re-list and retry), so a stale caller can never toggle the wrong task.
/// The caller commits the returned content via `NoteWriter::update_note`.
pub fn toggled_content(
    note_id: &str,
    content: &str,
    line: usize,
    expect_text: &str,
) -> Result<String, TacitusError> {
    let lines: Vec<&str> = content.lines().collect();
    let conflict = |reason: String| {
        TacitusError::new(
            "CONFLICT",
            reason,
            "The note changed since you listed tasks; call list_tasks again.",
        )
    };
    let raw_line = *lines
        .get(line)
        .ok_or_else(|| conflict(format!("Line {line} no longer exists in {note_id:?}.")))?;
    let task = parse_task_line(note_id, line, raw_line)
        .ok_or_else(|| conflict(format!("Line {line} in {note_id:?} is not a task.")))?;
    if task.text != expect_text {
        return Err(conflict(format!(
            "Task at line {line} is now {:?}, expected {expect_text:?}.",
            task.text
        )));
    }

    let toggled = if task.done {
        raw_line.replacen("[x]", "[ ]", 1).replacen("[X]", "[ ]", 1)
    } else {
        raw_line.replacen("[ ]", "[x]", 1)
    };
    let mut new_lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    new_lines[line] = toggled;
    Ok(new_lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::write::{NoteWriter, PermissionScope};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_vault() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-tasks-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("today.md"),
            "# Today\n\n- [ ] ship the release due:2026-07-25 #launch\n- [x] write tests\n- [ ] water plants\n",
        )
        .unwrap();
        fs::write(
            dir.join("plans.md"),
            "---\ntitle: Plans\n---\n- [ ] book venue \u{1F4C5} 2026-08-10 #launch\n* [X] draft agenda\nNot a task line.\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn parses_status_due_and_tags_across_notes() {
        let dir = make_vault();
        let index = VaultIndex::build(&dir).unwrap();
        let all = list_tasks(&index, &TaskFilter::default());
        assert_eq!(all.len(), 5);

        let ship = all.iter().find(|t| t.text.contains("ship")).unwrap();
        assert!(!ship.done);
        assert_eq!(ship.due.as_deref(), Some("2026-07-25"));
        assert_eq!(ship.tags, vec!["launch"]);

        // Emoji due syntax + `* [X]` checkbox variant both parse.
        let venue = all.iter().find(|t| t.text.contains("venue")).unwrap();
        assert_eq!(venue.due.as_deref(), Some("2026-08-10"));
        assert!(all.iter().any(|t| t.text.contains("agenda") && t.done));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn filters_and_due_ordering() {
        let dir = make_vault();
        let index = VaultIndex::build(&dir).unwrap();

        let open = list_tasks(
            &index,
            &TaskFilter {
                done: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(open.len(), 3);
        // Due-dated tasks first (ascending), undated last.
        assert!(open[0].text.contains("ship"));
        assert!(open[1].text.contains("venue"));
        assert!(open[2].due.is_none());

        let before = list_tasks(
            &index,
            &TaskFilter {
                due_before: Some("2026-08-01".into()),
                ..Default::default()
            },
        );
        assert_eq!(before.len(), 1);
        assert!(before[0].text.contains("ship"));

        let tagged = list_tasks(
            &index,
            &TaskFilter {
                tag: Some("launch".into()),
                ..Default::default()
            },
        );
        assert_eq!(tagged.len(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn token_budget_caps_the_list() {
        let dir = make_vault();
        let index = VaultIndex::build(&dir).unwrap();
        let all = list_tasks(&index, &TaskFilter::default());
        let total: usize = all.iter().map(|t| t.token_count).sum();
        let capped = list_tasks(
            &index,
            &TaskFilter {
                token_budget: Some(total / 2),
                ..Default::default()
            },
        );
        assert!(!capped.is_empty());
        assert!(capped.len() < all.len());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn toggle_roundtrip_is_versioned_and_reflected() {
        let dir = make_vault();
        let index = Arc::new(Mutex::new(VaultIndex::build(&dir).unwrap()));
        let mut writer = NoteWriter::with_index(&dir, PermissionScope::ReadWrite, index.clone());

        // Snapshot under the lock, compute purely, commit with the lock
        // released — the writer re-locks the index to reflect the change.
        let (task, content) = {
            let idx = index.lock().unwrap();
            let task = list_tasks(&idx, &TaskFilter::default())
                .into_iter()
                .find(|t| t.text.contains("water"))
                .unwrap();
            let content = idx.get(&task.note_id).unwrap().content.clone();
            (task, content)
        };
        let new_content = toggled_content(&task.note_id, &content, task.line, &task.text).unwrap();
        let result = writer
            .update_note(&task.note_id, Some(new_content), None)
            .unwrap();
        assert!(result.version_id.starts_with("v_"));
        assert!(fs::read_to_string(dir.join("today.md"))
            .unwrap()
            .contains("- [x] water plants"));
        // Live index reflects the flip; the write is audited.
        let idx = index.lock().unwrap();
        let again = list_tasks(&idx, &TaskFilter::default());
        assert!(again.iter().any(|t| t.text.contains("water") && t.done));
        assert!(dir.join(".tacitus").join("audit.log").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn toggle_with_stale_text_is_a_conflict() {
        let content = "- [ ] real task\nplain line\n";
        let err = toggled_content("today", content, 0, "something else entirely").unwrap_err();
        assert_eq!(err.code, "CONFLICT");
        let err = toggled_content("today", content, 1, "plain line").unwrap_err();
        assert_eq!(err.code, "CONFLICT"); // not a task line
        let err = toggled_content("today", content, 99, "x").unwrap_err();
        assert_eq!(err.code, "CONFLICT"); // line out of range
                                          // And a done task flips back to open.
        let back = toggled_content("t", "- [x] done thing", 0, "done thing").unwrap();
        assert_eq!(back, "- [ ] done thing");
    }
}
