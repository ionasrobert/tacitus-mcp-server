//! Declared note types (the "objects with a type" idea, agent-native): a file
//! `.tacitus/types/<name>.yaml` declares the properties a note of that type
//! carries —
//!
//! ```yaml
//! description: An article idea
//! properties:
//!   status: { type: string, required: true, enum: [idea, draft, published] }
//!   priority: { type: number }
//!   due: { type: date }
//!   topics: { type: list }
//! ```
//!
//! Any create/update whose resulting frontmatter has `type: <name>` for a
//! declared type is validated in `NoteWriter::build_diff` (so at propose AND at
//! commit), and a violation is a SCHEMA_VIOLATION listing every problem at
//! once — an agent fixes the call in one retry. Undeclared types are not
//! validated (existing vaults keep working), extra properties are allowed
//! (the schema is open), and the sync op SetRaw is never validated (merged
//! peer state must always land).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use serde_yaml::Value;

use crate::error::TacitusError;
use crate::ids::is_plain_id;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PropKind {
    String,
    Number,
    Boolean,
    /// `YYYY-MM-DD`, optionally followed by a time (`T10:00`, `T10:00:00Z`…).
    Date,
    List,
}

impl PropKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Date => "date",
            Self::List => "list",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropSpec {
    #[serde(rename = "type")]
    pub kind: PropKind,
    #[serde(default)]
    pub required: bool,
    #[serde(rename = "enum", default)]
    pub allowed: Option<Vec<Value>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeDef {
    #[serde(skip)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, PropSpec>,
}

fn date_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\d{4}-\d{2}-\d{2}([T ][0-9:.]+(Z|[+-]\d{2}:?\d{2})?)?$").unwrap()
    })
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Sequence(_) => "list",
        Value::Mapping(_) => "mapping",
        Value::Tagged(_) => "tagged",
    }
}

fn matches_kind(kind: PropKind, v: &Value) -> bool {
    match kind {
        PropKind::String => v.is_string(),
        PropKind::Number => v.is_number(),
        PropKind::Boolean => v.is_bool(),
        PropKind::Date => v.as_str().is_some_and(|s| date_re().is_match(s)),
        PropKind::List => v.is_sequence(),
    }
}

/// YAML scalar for messages: strings quoted, everything else as YAML.
fn show(v: &Value) -> String {
    match v {
        Value::String(s) => format!("{s:?}"),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

impl TypeDef {
    /// Every way `frontmatter` falls short of this type, in property order.
    /// A null value counts as absent.
    pub fn violations(&self, frontmatter: &Value) -> Vec<String> {
        let mut out = Vec::new();
        for (key, spec) in &self.properties {
            let value = frontmatter.get(key.as_str()).filter(|v| !v.is_null());
            let Some(value) = value else {
                if spec.required {
                    out.push(format!("missing required property {key:?}"));
                }
                continue;
            };
            if !matches_kind(spec.kind, value) {
                let hint = if spec.kind == PropKind::Date {
                    " (YYYY-MM-DD)"
                } else {
                    ""
                };
                out.push(format!(
                    "{key:?} must be {}{hint}, got {} {}",
                    spec.kind.as_str(),
                    kind_of(value),
                    show(value)
                ));
                continue;
            }
            if let Some(allowed) = &spec.allowed {
                if !allowed.contains(value) {
                    let options: Vec<String> = allowed.iter().map(show).collect();
                    out.push(format!(
                        "{key:?} must be one of [{}], got {}",
                        options.join(", "),
                        show(value)
                    ));
                }
            }
        }
        out
    }

    /// The agent-facing description of this type (list_types row).
    pub fn to_json(&self) -> JsonValue {
        let properties: serde_json::Map<String, JsonValue> = self
            .properties
            .iter()
            .map(|(k, spec)| {
                let mut p = json!({ "type": spec.kind.as_str(), "required": spec.required });
                if let Some(allowed) = &spec.allowed {
                    p["enum"] = serde_json::to_value(allowed).unwrap_or(JsonValue::Null);
                }
                (k.clone(), p)
            })
            .collect();
        json!({ "name": self.name, "description": self.description, "properties": properties })
    }
}

/// Type declarations on disk under `.tacitus/types/*.yaml` (or `.yml`).
pub struct TypeStore {
    dir: PathBuf,
}

/// One entry of [`TypeStore::list`]: a parsed type, or why its file is broken.
pub enum TypeEntry {
    Ok(TypeDef),
    Broken { name: String, error: String },
}

impl TypeStore {
    pub fn new(vault_dir: impl AsRef<Path>) -> Self {
        Self {
            dir: vault_dir.as_ref().join(".tacitus").join("types"),
        }
    }

    fn parse(name: &str, raw: &str) -> Result<TypeDef, String> {
        let mut def: TypeDef = serde_yaml::from_str(raw).map_err(|e| e.to_string())?;
        def.name = name.to_string();
        Ok(def)
    }

    fn file(&self, name: &str) -> Option<PathBuf> {
        ["yaml", "yml"]
            .iter()
            .map(|ext| self.dir.join(format!("{name}.{ext}")))
            .find(|p| p.is_file())
    }

    /// The declared type `name`, None if undeclared. A declared but malformed
    /// type is an INVALID_TYPE_DEF error — never silently "no validation".
    pub fn load(&self, name: &str) -> Result<Option<TypeDef>, TacitusError> {
        if !is_plain_id(name) {
            return Ok(None); // can't be a file stem we'd read
        }
        let Some(path) = self.file(name) else {
            return Ok(None);
        };
        let raw = fs::read_to_string(&path)?;
        Self::parse(name, &raw).map(Some).map_err(|e| {
            TacitusError::new(
                "INVALID_TYPE_DEF",
                format!("Type {name:?} ({}) is malformed: {e}", path.display()),
                "Fix the type file: properties map to {type: string|number|boolean|date|list, required?, enum?}.",
            )
        })
    }

    /// Every type file, sorted by name; broken ones are reported, not hidden.
    pub fn list(&self) -> std::io::Result<Vec<TypeEntry>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(err) => return Err(err),
        };
        // Stems first, then resolve each through `file` — so `.yaml` wins over
        // `.yml` exactly as in `load`, whatever order read_dir yields.
        let mut names = std::collections::BTreeSet::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("yaml" | "yml")) {
                continue;
            }
            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                if is_plain_id(name) {
                    names.insert(name.to_string());
                }
            }
        }
        let mut out = Vec::new();
        for name in names {
            let Some(path) = self.file(&name) else {
                continue;
            };
            let parsed = fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|raw| Self::parse(&name, &raw));
            out.push(match parsed {
                Ok(def) => TypeEntry::Ok(def),
                Err(error) => TypeEntry::Broken { name, error },
            });
        }
        Ok(out)
    }

    /// Validate a note's resulting frontmatter against its declared type (if
    /// any). Ok when the note is untyped or its type is undeclared.
    ///
    /// `before` is the note's prior frontmatter on an update: only problems
    /// the write INTRODUCES are refused, so a note that predates its type
    /// declaration can still be tagged or linked — it just can't get worse.
    pub fn check(
        &self,
        note_id: &str,
        frontmatter: &Value,
        before: Option<&Value>,
    ) -> Result<(), TacitusError> {
        let Some(type_name) = frontmatter.get("type").and_then(|v| v.as_str()) else {
            return Ok(());
        };
        let Some(def) = self.load(type_name)? else {
            return Ok(());
        };
        let existing = before.map(|fm| def.violations(fm)).unwrap_or_default();
        let problems: Vec<String> = def
            .violations(frontmatter)
            .into_iter()
            .filter(|p| !existing.contains(p))
            .collect();
        if problems.is_empty() {
            return Ok(());
        }
        Err(TacitusError::new(
            "SCHEMA_VIOLATION",
            format!(
                "Note {note_id:?} (type {type_name:?}): {}.",
                problems.join("; ")
            ),
            format!(
                "Fix the frontmatter to match .tacitus/types/{type_name}.yaml (see list_types)."
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(defs: &[(&str, &str)]) -> (PathBuf, TypeStore) {
        let dir =
            std::env::temp_dir().join(format!("tacitus-schema-{}", crate::test_unique_suffix()));
        fs::create_dir_all(dir.join(".tacitus/types")).unwrap();
        for (file, body) in defs {
            fs::write(dir.join(".tacitus/types").join(file), body).unwrap();
        }
        let store = TypeStore::new(&dir);
        (dir, store)
    }

    fn fm(yaml: &str) -> Value {
        serde_yaml::from_str(yaml).unwrap()
    }

    const ARTICLE: &str = "description: An article idea\nproperties:\n  status: { type: string, required: true, enum: [idea, draft, published] }\n  priority: { type: number }\n  due: { type: date }\n  topics: { type: list }\n  pinned: { type: boolean }\n";

    #[test]
    fn conforming_and_untyped_and_undeclared_notes_pass() {
        let (_d, store) = store_with(&[("article.yaml", ARTICLE)]);
        store
            .check("a", &fm("type: article\nstatus: draft\npriority: 2\ndue: 2026-11-14\ntopics: [x]\npinned: true\nextra: ok\n"), None)
            .unwrap();
        store.check("b", &fm("title: no type\n"), None).unwrap();
        store.check("c", &fm("type: unknown\n"), None).unwrap();
        store.check("d", &fm("type: ../article\n"), None).unwrap();
        store
            .check(
                "e",
                &fm("type: article\nstatus: idea\ndue: 2026-11-14T10:30:00Z\n"),
                None,
            )
            .unwrap();
    }

    #[test]
    fn every_violation_is_listed_at_once() {
        let (_d, store) = store_with(&[("article.yaml", ARTICLE)]);
        let err = store
            .check(
                "notes/x",
                &fm(
                    "type: article\npriority: high\ndue: next week\ntopics: one\npinned: yes-ish\n",
                ),
                None,
            )
            .unwrap_err();
        assert_eq!(err.code, "SCHEMA_VIOLATION");
        for needle in [
            "missing required property \"status\"",
            "\"priority\" must be number, got string \"high\"",
            "\"due\" must be date (YYYY-MM-DD)",
            "\"topics\" must be list",
            "\"pinned\" must be boolean",
        ] {
            assert!(err.reason.contains(needle), "{needle} in {}", err.reason);
        }
        let err = store
            .check("y", &fm("type: article\nstatus: shipped\n"), None)
            .unwrap_err();
        assert!(err
            .reason
            .contains("must be one of [\"idea\", \"draft\", \"published\"]"));
        // null counts as absent
        let err = store
            .check("z", &fm("type: article\nstatus: null\n"), None)
            .unwrap_err();
        assert!(err.reason.contains("missing required property \"status\""));
    }

    #[test]
    fn malformed_type_files_fail_loudly_and_list_reports_them() {
        let (_d, store) = store_with(&[
            ("article.yml", ARTICLE),
            ("bad.yaml", "properties:\n  x: { type: colour }\n"),
            ("notes.txt", "ignored"),
        ]);
        let err = store.check("n", &fm("type: bad\n"), None).unwrap_err();
        assert_eq!(err.code, "INVALID_TYPE_DEF");
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert!(matches!(&listed[0], TypeEntry::Ok(d) if d.name == "article"));
        assert!(matches!(&listed[1], TypeEntry::Broken { name, .. } if name == "bad"));
    }
}
