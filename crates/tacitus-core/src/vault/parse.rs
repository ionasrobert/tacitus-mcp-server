use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;
use serde_yaml::Value;

use super::types::{Heading, Note, WikiLink};

fn wikilink_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[\[([^\]]+)\]\]").unwrap())
}

fn tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|\s)#([A-Za-z0-9_/-]+)").unwrap())
}

fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in text.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(c);
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    slug
}

fn split_frontmatter(raw: &str) -> (Value, String) {
    let empty = || Value::Mapping(Default::default());
    if let Some(rest) = raw.strip_prefix("---\n") {
        if let Some(idx) = rest.find("\n---") {
            let fm = serde_yaml::from_str::<Value>(&rest[..idx]).unwrap_or_else(|_| empty());
            let after = &rest[idx + 4..];
            let body = after.strip_prefix('\n').unwrap_or(after);
            return (fm, body.to_string());
        }
    }
    (empty(), raw.to_string())
}

fn parse_heading(line: &str) -> Option<Heading> {
    let level = line.chars().take_while(|&c| c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &line[level..];
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let text = rest.trim();
    if text.is_empty() {
        return None;
    }
    Some(Heading {
        level,
        text: text.to_string(),
        slug: slugify(text),
    })
}

fn extract_wikilinks(body: &str) -> Vec<WikiLink> {
    wikilink_re()
        .captures_iter(body)
        .map(|cap| {
            let raw = cap.get(0).map_or("", |m| m.as_str()).to_string();
            let inner = cap.get(1).map_or("", |m| m.as_str());
            let (link_part, alias) = match inner.split_once('|') {
                Some((l, a)) => (l, Some(a.trim().to_string())),
                None => (inner, None),
            };
            let mut link = WikiLink {
                raw,
                alias,
                ..Default::default()
            };
            if let Some(hash) = link_part.find('#') {
                link.target = link_part[..hash].trim().to_string();
                let after = &link_part[hash + 1..];
                if let Some(block) = after.strip_prefix('^') {
                    link.block = Some(block.trim().to_string());
                } else {
                    link.heading = Some(after.trim().to_string());
                }
            } else {
                link.target = link_part.trim().to_string();
            }
            link
        })
        .collect()
}

fn frontmatter_tags(fm: &Value) -> Vec<String> {
    match fm.get("tags") {
        Some(Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(Value::String(s)) => s
            .split([',', ' ', '\t'])
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect(),
        _ => vec![],
    }
}

fn extract_tags(body: &str, fm: &Value) -> Vec<String> {
    let mut tags = Vec::new();
    let mut seen = HashSet::new();
    for tag in frontmatter_tags(fm) {
        if seen.insert(tag.clone()) {
            tags.push(tag);
        }
    }
    for cap in tag_re().captures_iter(body) {
        let tag = cap.get(1).map_or("", |m| m.as_str()).to_string();
        if seen.insert(tag.clone()) {
            tags.push(tag);
        }
    }
    tags
}

/// Parse a raw `.md` file into a machine-readable Note.
pub fn parse_note(raw: &str, rel_path: &str) -> Note {
    let id = rel_path.strip_suffix(".md").unwrap_or(rel_path).to_string();
    let (frontmatter, body) = split_frontmatter(raw);
    let headings: Vec<Heading> = body.lines().filter_map(parse_heading).collect();

    let title = frontmatter
        .get("title")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            headings
                .iter()
                .find(|h| h.level == 1)
                .map(|h| h.text.clone())
        })
        .unwrap_or_else(|| id.rsplit('/').next().unwrap_or(&id).to_string());

    let links = extract_wikilinks(&body);
    let tags = extract_tags(&body, &frontmatter);

    Note {
        id,
        path: rel_path.to_string(),
        title,
        frontmatter,
        content: body,
        headings,
        links,
        tags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "---\ntitle: Launch Plan\ntags: [project, q1]\n---\n# Launch Plan\n\nSee [[projects/timeline|the timeline]] and [[Risks#Mitigations]].\nAlso [[Notes#^block1]]. Inline #urgent tag.\n\n## Timeline\n## Risks\n";

    #[test]
    fn extracts_id_title_and_body() {
        let note = parse_note(RAW, "projects/launch.md");
        assert_eq!(note.id, "projects/launch");
        assert_eq!(note.title, "Launch Plan");
        assert!(note.content.contains("See [[projects/timeline"));
        assert!(!note.content.contains("title: Launch Plan"));
    }

    #[test]
    fn extracts_headings_with_slugs() {
        let note = parse_note(RAW, "a.md");
        let texts: Vec<&str> = note.headings.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, vec!["Launch Plan", "Timeline", "Risks"]);
        assert_eq!(note.headings[0].slug, "launch-plan");
    }

    #[test]
    fn extracts_wikilinks_with_alias_heading_block() {
        let note = parse_note(RAW, "a.md");
        let by_target: std::collections::HashMap<&str, &WikiLink> =
            note.links.iter().map(|l| (l.target.as_str(), l)).collect();
        assert_eq!(
            by_target["projects/timeline"].alias.as_deref(),
            Some("the timeline")
        );
        assert_eq!(by_target["Risks"].heading.as_deref(), Some("Mitigations"));
        assert_eq!(by_target["Notes"].block.as_deref(), Some("block1"));
    }

    #[test]
    fn collects_frontmatter_and_inline_tags() {
        let mut tags = parse_note(RAW, "a.md").tags;
        tags.sort();
        assert_eq!(tags, vec!["project", "q1", "urgent"]);
    }

    #[test]
    fn falls_back_to_h1_then_filename_for_title() {
        assert_eq!(
            parse_note("# Only Heading\n\ntext", "foo/bar.md").title,
            "Only Heading"
        );
        assert_eq!(parse_note("no heading here", "foo/baz.md").title, "baz");
    }
}
