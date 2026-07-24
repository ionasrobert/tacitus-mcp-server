//! Bases-like structured queries over typed frontmatter (Part I.3 / III):
//! `properties_query` treats the vault's YAML properties as a queryable
//! database — "all notes where status=active and due < 2026-08-01" is a
//! query, not a fuzzy search. Output is bounded (limit + token_budget) and
//! every row carries its own token cost, per the golden rule for tools.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::index::VaultIndex;
use crate::tokens::estimate;

/// A single filter condition. All of a query's filters are AND-ed; an agent
/// wanting OR runs the query twice.
#[derive(Clone, Debug)]
pub struct PropFilter {
    pub key: String,
    pub op: PropOp,
    /// Comparison value; unused for Exists / NotExists.
    pub value: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropOp {
    /// Scalar equality (numbers compare numerically).
    Eq,
    Ne,
    /// Array membership, or case-insensitive substring for strings.
    Contains,
    Exists,
    NotExists,
    /// Ordered comparisons: numeric when both sides are numbers, else
    /// lexicographic — which makes ISO dates ("2026-07-24") work naturally.
    Gt,
    Lt,
    Gte,
    Lte,
}

impl PropOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "eq" => Some(Self::Eq),
            "ne" => Some(Self::Ne),
            "contains" => Some(Self::Contains),
            "exists" => Some(Self::Exists),
            "not_exists" => Some(Self::NotExists),
            "gt" => Some(Self::Gt),
            "lt" => Some(Self::Lt),
            "gte" => Some(Self::Gte),
            "lte" => Some(Self::Lte),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PropertiesQueryArgs {
    pub filters: Vec<PropFilter>,
    /// Property keys to include in each row (default: all).
    pub select: Option<Vec<String>>,
    /// Sort by this property's value (missing values sort last).
    pub sort_by: Option<String>,
    pub descending: bool,
    /// Max rows (default 50).
    pub limit: Option<usize>,
    /// Hard token ceiling across all returned rows.
    pub token_budget: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct PropertiesRow {
    pub note_id: String,
    pub title: String,
    pub properties: Map<String, Value>,
    pub token_count: usize,
}

const DEFAULT_LIMIT: usize = 50;

/// Frontmatter as a JSON object (non-mapping / non-string-keyed frontmatter
/// yields an empty object rather than an error).
fn json_properties(fm: &serde_yaml::Value) -> Map<String, Value> {
    match serde_json::to_value(fm) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn json_eq(a: &Value, b: &Value) -> bool {
    match (as_f64(a), as_f64(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// Ordered comparison; None when the two values aren't comparable.
fn json_cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    if let (Some(x), Some(y)) = (as_f64(a), as_f64(b)) {
        return x.partial_cmp(&y);
    }
    match (a.as_str(), b.as_str()) {
        (Some(x), Some(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn matches(props: &Map<String, Value>, filter: &PropFilter) -> bool {
    let actual = props.get(&filter.key);
    match filter.op {
        PropOp::Exists => actual.is_some(),
        PropOp::NotExists => actual.is_none(),
        _ => {
            let (Some(actual), Some(expected)) = (actual, filter.value.as_ref()) else {
                return false;
            };
            match filter.op {
                PropOp::Eq => json_eq(actual, expected),
                PropOp::Ne => !json_eq(actual, expected),
                PropOp::Contains => match actual {
                    Value::Array(items) => items.iter().any(|item| json_eq(item, expected)),
                    Value::String(s) => expected
                        .as_str()
                        .is_some_and(|needle| s.to_lowercase().contains(&needle.to_lowercase())),
                    _ => false,
                },
                PropOp::Gt => json_cmp(actual, expected) == Some(std::cmp::Ordering::Greater),
                PropOp::Lt => json_cmp(actual, expected) == Some(std::cmp::Ordering::Less),
                PropOp::Gte => matches!(
                    json_cmp(actual, expected),
                    Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
                ),
                PropOp::Lte => matches!(
                    json_cmp(actual, expected),
                    Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                ),
                PropOp::Exists | PropOp::NotExists => unreachable!(),
            }
        }
    }
}

/// Query notes by their typed frontmatter properties. Deterministic: rows are
/// ordered by `sort_by` (or note_id), then capped by `limit` and
/// `token_budget`.
pub fn properties_query(index: &VaultIndex, args: &PropertiesQueryArgs) -> Vec<PropertiesRow> {
    // BTreeMap for a deterministic base order by note_id.
    let mut matched: BTreeMap<String, (String, Map<String, Value>)> = BTreeMap::new();
    for note in index.all() {
        let props = json_properties(&note.frontmatter);
        if args.filters.iter().all(|f| matches(&props, f)) {
            matched.insert(note.id.clone(), (note.title.clone(), props));
        }
    }

    let mut rows: Vec<(String, String, Map<String, Value>)> = matched
        .into_iter()
        .map(|(id, (title, props))| (id, title, props))
        .collect();

    if let Some(sort_key) = &args.sort_by {
        rows.sort_by(|a, b| {
            let cmp = match (a.2.get(sort_key), b.2.get(sort_key)) {
                (Some(x), Some(y)) => json_cmp(x, y).unwrap_or(std::cmp::Ordering::Equal),
                (Some(_), None) => std::cmp::Ordering::Less, // missing sorts last
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            };
            if args.descending {
                cmp.reverse()
            } else {
                cmp
            }
        });
    }

    rows.truncate(args.limit.unwrap_or(DEFAULT_LIMIT));

    let budget = args.token_budget.unwrap_or(usize::MAX);
    let mut used = 0usize;
    let mut out = Vec::new();
    for (note_id, title, mut properties) in rows {
        if let Some(keys) = &args.select {
            properties.retain(|k, _| keys.iter().any(|s| s == k));
        }
        let token_count = estimate(
            &serde_json::to_string(&serde_json::json!({
                "note_id": note_id, "title": title, "properties": properties
            }))
            .unwrap_or_default(),
        );
        if used + token_count > budget {
            continue;
        }
        used += token_count;
        out.push(PropertiesRow {
            note_id,
            title,
            properties,
            token_count,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_vault() -> (PathBuf, VaultIndex) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-props-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("alpha.md"),
            "---\nstatus: active\npriority: 3\ndue: 2026-08-01\ntags: [project, q3]\n---\nAlpha.\n",
        )
        .unwrap();
        fs::write(
            dir.join("beta.md"),
            "---\nstatus: done\npriority: 1\ndue: 2026-07-01\ntags: [project]\n---\nBeta.\n",
        )
        .unwrap();
        fs::write(
            dir.join("gamma.md"),
            "---\nstatus: active\npriority: 2\n---\nGamma (no due, no tags).\n",
        )
        .unwrap();
        fs::write(dir.join("plain.md"), "No frontmatter at all.\n").unwrap();
        let index = VaultIndex::build(&dir).unwrap();
        (dir, index)
    }

    fn ids(rows: &[PropertiesRow]) -> Vec<&str> {
        rows.iter().map(|r| r.note_id.as_str()).collect()
    }

    fn filter(key: &str, op: PropOp, value: Option<Value>) -> PropFilter {
        PropFilter {
            key: key.into(),
            op,
            value,
        }
    }

    #[test]
    fn eq_and_array_contains_are_anded() {
        let (dir, index) = make_vault();
        let rows = properties_query(
            &index,
            &PropertiesQueryArgs {
                filters: vec![
                    filter("status", PropOp::Eq, Some("active".into())),
                    filter("tags", PropOp::Contains, Some("project".into())),
                ],
                ..Default::default()
            },
        );
        assert_eq!(ids(&rows), vec!["alpha"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn numeric_comparison_sort_and_limit() {
        let (dir, index) = make_vault();
        let rows = properties_query(
            &index,
            &PropertiesQueryArgs {
                filters: vec![filter("priority", PropOp::Gte, Some(1.into()))],
                sort_by: Some("priority".into()),
                descending: true,
                limit: Some(2),
                ..Default::default()
            },
        );
        assert_eq!(ids(&rows), vec!["alpha", "gamma"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn iso_date_strings_compare_lexicographically() {
        let (dir, index) = make_vault();
        let rows = properties_query(
            &index,
            &PropertiesQueryArgs {
                filters: vec![filter("due", PropOp::Lt, Some("2026-07-15".into()))],
                ..Default::default()
            },
        );
        assert_eq!(ids(&rows), vec!["beta"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exists_not_exists_and_select_projection() {
        let (dir, index) = make_vault();
        let rows = properties_query(
            &index,
            &PropertiesQueryArgs {
                filters: vec![
                    filter("status", PropOp::Exists, None),
                    filter("due", PropOp::NotExists, None),
                ],
                select: Some(vec!["status".into()]),
                ..Default::default()
            },
        );
        assert_eq!(ids(&rows), vec!["gamma"]);
        assert_eq!(rows[0].properties.len(), 1);
        assert_eq!(rows[0].properties.get("status"), Some(&"active".into()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn token_budget_is_a_hard_ceiling() {
        let (dir, index) = make_vault();
        let all = properties_query(&index, &PropertiesQueryArgs::default());
        assert_eq!(all.len(), 4); // includes the empty-frontmatter note
        let total: usize = all.iter().map(|r| r.token_count).sum();

        let capped = properties_query(
            &index,
            &PropertiesQueryArgs {
                token_budget: Some(total / 2),
                ..Default::default()
            },
        );
        assert!(!capped.is_empty());
        assert!(capped.len() < all.len());
        assert!(capped.iter().map(|r| r.token_count).sum::<usize>() <= total / 2);
        fs::remove_dir_all(&dir).ok();
    }
}
