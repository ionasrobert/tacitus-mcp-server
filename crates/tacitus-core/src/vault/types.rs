use serde_yaml::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: usize,
    pub text: String,
    pub slug: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WikiLink {
    pub target: String,
    pub heading: Option<String>,
    pub block: Option<String>,
    pub alias: Option<String>,
    pub raw: String,
}

/// A parsed vault note — the machine-readable view an agent queries.
#[derive(Debug, Clone)]
pub struct Note {
    /// Stable id: the relative path without extension, e.g. "projects/launch".
    pub id: String,
    pub path: String,
    pub title: String,
    pub frontmatter: Value,
    pub content: String,
    pub headings: Vec<Heading>,
    pub links: Vec<WikiLink>,
    pub tags: Vec<String>,
}
