//! Templates with schema (the "Templater/QuickAdd" capability, agent-native):
//! a template is a Markdown file in `.tacitus/templates/` whose `{{var}}`
//! placeholders form its schema. Rendering substitutes on the RAW text —
//! frontmatter included — so `priority: {{p}}` with p=3 parses back as a real
//! YAML number, guaranteeing agent-created notes land with typed properties.
//! Builtins `{{date}}`, `{{time}}`, `{{datetime}}` auto-fill; anything else
//! missing is a structured MISSING_VARS error naming exactly what to supply.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::error::TacitusError;

const BUILTINS: [&str; 3] = ["date", "time", "datetime"];

fn placeholder_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\{\{\s*([A-Za-z0-9_]+)\s*\}\}").unwrap())
}

/// A discovered template: its name plus the variables an agent must supply
/// (builtins excluded — they auto-fill).
#[derive(Clone, Debug)]
pub struct Template {
    pub name: String,
    pub vars: Vec<String>,
}

/// Unique non-builtin placeholders, in order of first appearance.
pub fn template_vars(raw: &str) -> Vec<String> {
    let mut vars = Vec::new();
    for cap in placeholder_re().captures_iter(raw) {
        let name = cap.get(1).map_or("", |m| m.as_str());
        if !BUILTINS.contains(&name) && !vars.iter().any(|v| v == name) {
            vars.push(name.to_string());
        }
    }
    vars
}

/// Substitute `{{var}}` placeholders. Caller-supplied vars win over builtins;
/// any other unresolved placeholder is a MISSING_VARS error listing all of
/// them, so an agent can fix the call in one retry.
pub fn render_template(raw: &str, vars: &HashMap<String, String>) -> Result<String, TacitusError> {
    let missing: Vec<String> = template_vars(raw)
        .into_iter()
        .filter(|v| !vars.contains_key(v))
        .collect();
    if !missing.is_empty() {
        return Err(TacitusError::new(
            "MISSING_VARS",
            format!("Template requires vars: {}.", missing.join(", ")),
            "Pass every listed var in the vars object.",
        ));
    }

    let now = chrono::Local::now();
    let builtin = |name: &str| -> String {
        match name {
            "date" => now.format("%Y-%m-%d").to_string(),
            "time" => now.format("%H:%M").to_string(),
            _ => now.to_rfc3339(),
        }
    };

    Ok(placeholder_re()
        .replace_all(raw, |cap: &regex::Captures| {
            let name = cap.get(1).map_or("", |m| m.as_str());
            vars.get(name).cloned().unwrap_or_else(|| builtin(name))
        })
        .into_owned())
}

/// Templates on disk under `.tacitus/templates/*.md`.
pub struct TemplateStore {
    dir: PathBuf,
}

impl TemplateStore {
    pub fn new(vault_dir: impl AsRef<Path>) -> Self {
        Self {
            dir: vault_dir.as_ref().join(".tacitus").join("templates"),
        }
    }

    /// All templates, sorted by name, each with its inferred vars.
    pub fn list(&self) -> std::io::Result<Vec<Template>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(err) => return Err(err),
        };
        let mut templates = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(raw) = fs::read_to_string(&path) {
                templates.push(Template {
                    name: name.to_string(),
                    vars: template_vars(&raw),
                });
            }
        }
        templates.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(templates)
    }

    /// Raw template contents; the name must be a bare file stem.
    pub fn load_raw(&self, name: &str) -> Result<String, TacitusError> {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(TacitusError::new(
                "TEMPLATE_NOT_FOUND",
                format!("Invalid template name {name:?}."),
                "Use a bare template name from list_templates.",
            ));
        }
        fs::read_to_string(self.dir.join(format!("{name}.md"))).map_err(|_| {
            TacitusError::new(
                "TEMPLATE_NOT_FOUND",
                format!("No template named {name:?}."),
                "Use list_templates to see available templates.",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::parse_note;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_vault() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-tpl-{nanos}"));
        fs::create_dir_all(dir.join(".tacitus").join("templates")).unwrap();
        dir
    }

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn renders_vars_and_autofills_builtin_date() {
        let raw = "# {{title}}\n\nCreated: {{date}}\n";
        let out = render_template(raw, &vars(&[("title", "Standup")])).unwrap();
        assert!(out.contains("# Standup"));
        // {{date}} auto-filled as YYYY-MM-DD without being passed.
        assert!(regex::Regex::new(r"Created: \d{4}-\d{2}-\d{2}\n")
            .unwrap()
            .is_match(&out));
    }

    #[test]
    fn missing_vars_is_a_structured_error_naming_them() {
        let raw = "{{title}} by {{author}} on {{date}}";
        let err = render_template(raw, &vars(&[("title", "x")])).unwrap_err();
        assert_eq!(err.code, "MISSING_VARS");
        assert!(err.reason.contains("author"));
        assert!(!err.reason.contains("date")); // builtin never reported missing
    }

    #[test]
    fn substitution_before_parsing_preserves_yaml_types() {
        let raw = "---\ntitle: \"{{title}}\"\npriority: {{p}}\ndone: false\n---\nBody {{title}}.\n";
        let out = render_template(raw, &vars(&[("title", "Plan"), ("p", "3")])).unwrap();
        let note = parse_note(&out, "x.md");
        assert_eq!(
            note.frontmatter.get("priority").and_then(|v| v.as_u64()),
            Some(3)
        );
        assert_eq!(note.title, "Plan");
        assert!(note.content.contains("Body Plan."));
    }

    #[test]
    fn store_lists_templates_with_inferred_vars() {
        let dir = temp_vault();
        let tpl_dir = dir.join(".tacitus").join("templates");
        fs::write(
            tpl_dir.join("meeting.md"),
            "---\ntags: [meeting]\n---\n# {{title}}\n{{date}} with {{attendees}} re {{title}}\n",
        )
        .unwrap();
        fs::write(tpl_dir.join("blank.md"), "Nothing dynamic.\n").unwrap();
        let store = TemplateStore::new(&dir);
        let templates = store.list().unwrap();
        assert_eq!(
            templates
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["blank", "meeting"]
        );
        // vars: unique, first-appearance order, builtins excluded.
        assert_eq!(templates[1].vars, vec!["title", "attendees"]);
        assert!(templates[0].vars.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_raw_rejects_unknown_and_escaping_names() {
        let dir = temp_vault();
        let store = TemplateStore::new(&dir);
        assert_eq!(
            store.load_raw("nope").unwrap_err().code,
            "TEMPLATE_NOT_FOUND"
        );
        assert_eq!(
            store.load_raw("../../etc/passwd").unwrap_err().code,
            "TEMPLATE_NOT_FOUND"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
