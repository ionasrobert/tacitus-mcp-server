//! Quick capture into a daily note (`daily/YYYY-MM-DD`): the agent drops a
//! thought or a task without deciding where it belongs; sorting it out is a
//! later step. Pure helpers — the tool layer reads the note, builds the new
//! content here, and commits it through `NoteWriter` (versioned + audited).

use std::sync::OnceLock;

use regex::Regex;

use crate::error::TacitusError;

/// Folder the daily notes live in.
pub const DAILY_DIR: &str = "daily";
/// Template used for a new daily note when present (and needing no vars).
pub const DAILY_TEMPLATE: &str = "daily";

fn date_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{4}-(0[1-9]|1[0-2])-(0[1-9]|[12]\d|3[01])$").unwrap())
}

/// The daily note id for an ISO date, rejecting anything else.
pub fn daily_note_id(date: &str) -> Result<String, TacitusError> {
    if !date_re().is_match(date) {
        return Err(TacitusError::new(
            "INVALID_INPUT",
            format!("date must be YYYY-MM-DD (got {date:?})."),
            "Omit date for today, or pass an ISO date like 2026-11-14.",
        ));
    }
    Ok(format!("{DAILY_DIR}/{date}"))
}

/// The line a capture appends: `- [ ] text` for a task (so list_tasks sees
/// it), else `- HH:MM text`. Multi-line text is folded onto one line so the
/// entry stays a single list item.
pub fn capture_line(text: &str, task: bool, hh_mm: &str) -> Result<String, TacitusError> {
    let folded = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.is_empty() {
        return Err(TacitusError::new(
            "INVALID_INPUT",
            "text is empty.",
            "Pass the thought or task to capture in text.",
        ));
    }
    Ok(if task {
        format!("- [ ] {folded}")
    } else {
        format!("- {hh_mm} {folded}")
    })
}

/// `content` with `line` appended: directly under a trailing list, else after
/// a blank line so it starts a new list.
pub fn append_line(content: &str, line: &str) -> String {
    let body = content.trim_end();
    if body.is_empty() {
        return line.to_string();
    }
    let last = body.lines().last().unwrap_or("").trim_start();
    let in_list = last.starts_with("- ") || last.starts_with("* ");
    format!("{body}{}{line}", if in_list { "\n" } else { "\n\n" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_validated() {
        assert_eq!(daily_note_id("2026-11-14").unwrap(), "daily/2026-11-14");
        for bad in ["2026-13-01", "26-11-14", "../x", "2026-11-14x", ""] {
            assert_eq!(daily_note_id(bad).unwrap_err().code, "INVALID_INPUT");
        }
    }

    #[test]
    fn lines_are_single_line_bullets_or_tasks() {
        assert_eq!(
            capture_line("  idea\n about  X ", false, "09:05").unwrap(),
            "- 09:05 idea about X"
        );
        assert_eq!(
            capture_line("call Ana due:2026-11-20", true, "09:05").unwrap(),
            "- [ ] call Ana due:2026-11-20"
        );
        assert_eq!(
            capture_line(" \n ", false, "09:05").unwrap_err().code,
            "INVALID_INPUT"
        );
    }

    #[test]
    fn appends_under_a_list_or_after_a_blank_line() {
        assert_eq!(append_line("", "- a"), "- a");
        assert_eq!(append_line("# 2026-11-14\n", "- a"), "# 2026-11-14\n\n- a");
        assert_eq!(append_line("# D\n\n- a\n", "- b"), "# D\n\n- a\n- b");
        assert_eq!(
            append_line("# D\n\n- [ ] t\n\n", "- b"),
            "# D\n\n- [ ] t\n- b"
        );
    }
}
