use chrono::Utc;

use super::types::{Author, Memory, MemoryType, Provenance};
use crate::error::TacitusError;
use crate::ids::stable_id;

/// Provenance as provided by a caller — `timestamp` may be omitted (stamped here).
pub struct ProvenanceInput {
    pub origin: String,
    pub author: String,
    pub timestamp: Option<String>,
}

/// What a caller provides to `remember`. Provenance is optional at the type
/// level so we can return the precise MISSING_PROVENANCE error, like the TS port.
pub struct RememberInput {
    pub content: String,
    pub memory_type: String,
    pub tags: Vec<String>,
    pub key: Option<String>,
    pub source: Option<ProvenanceInput>,
    pub ttl: Option<u64>,
}

/// Validate an input, enforce mandatory provenance, and return a stored Memory
/// with a stable id. Idempotent: identical content+source ⇒ identical id (the
/// id seed excludes the timestamp).
pub fn remember(input: RememberInput) -> Result<Memory, TacitusError> {
    let source = input.source.ok_or_else(|| {
        TacitusError::new(
            "MISSING_PROVENANCE",
            "A memory must carry its source (provenance).",
            "Provide source: { origin, author: \"human\"|\"agent\", timestamp? }.",
        )
    })?;

    let memory_type = MemoryType::parse(&input.memory_type).ok_or_else(|| {
        TacitusError::new(
            "INVALID_TYPE",
            format!(
                "type must be one of user|feedback|project|reference (got {:?}).",
                input.memory_type
            ),
            "Use a valid memory type.",
        )
    })?;

    let author = Author::parse(&source.author).ok_or_else(|| {
        TacitusError::new(
            "INVALID_INPUT",
            "source.author must be \"human\" or \"agent\".",
            "Fix the author field and retry.",
        )
    })?;

    if input.content.is_empty() {
        return Err(TacitusError::new(
            "INVALID_INPUT",
            "content must be a non-empty string.",
            "Provide non-empty content.",
        ));
    }
    if source.origin.is_empty() {
        return Err(TacitusError::new(
            "INVALID_INPUT",
            "source.origin must be a non-empty string.",
            "Provide a provenance origin.",
        ));
    }

    let timestamp = match source.timestamp {
        Some(ts) if !ts.is_empty() => ts,
        _ => Utc::now().to_rfc3339(),
    };

    // Seed matches the TS engine exactly: "{type} {key} {content} {origin} {author}".
    let seed = format!(
        "{} {} {} {} {}",
        memory_type.as_str(),
        input.key.as_deref().unwrap_or(""),
        input.content,
        source.origin,
        author.as_str(),
    );

    Ok(Memory {
        id: stable_id(&seed, "mem"),
        memory_type,
        content: input.content,
        tags: input.tags,
        key: input.key,
        source: Provenance {
            origin: source.origin,
            author,
            timestamp,
        },
        ttl: input.ttl,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_source() -> ProvenanceInput {
        ProvenanceInput {
            origin: "chat".into(),
            author: "agent".into(),
            timestamp: Some("2026-07-23T10:00:00.000Z".into()),
        }
    }

    fn base_input() -> RememberInput {
        RememberInput {
            content: "hello".into(),
            memory_type: "user".into(),
            tags: vec![],
            key: None,
            source: Some(valid_source()),
            ttl: None,
        }
    }

    #[test]
    fn rejects_missing_provenance() {
        let mut input = base_input();
        input.source = None;
        assert_eq!(remember(input).unwrap_err().code, "MISSING_PROVENANCE");
    }

    #[test]
    fn rejects_invalid_type() {
        let mut input = base_input();
        input.memory_type = "nonsense".into();
        assert_eq!(remember(input).unwrap_err().code, "INVALID_TYPE");
    }

    #[test]
    fn assigns_stable_id_and_stamps_timestamp() {
        let mut input = base_input();
        input.source = Some(ProvenanceInput {
            origin: "chat".into(),
            author: "agent".into(),
            timestamp: None,
        });
        let memory = remember(input).unwrap();
        assert!(memory.id.starts_with("mem_"));
        assert_eq!(memory.id.len(), 20);
        assert!(!memory.source.timestamp.is_empty());
    }

    #[test]
    fn is_idempotent_for_identical_content_and_source() {
        assert_eq!(
            remember(base_input()).unwrap().id,
            remember(base_input()).unwrap().id
        );
    }

    #[test]
    fn different_content_yields_different_id() {
        let mut other = base_input();
        other.content = "different".into();
        assert_ne!(
            remember(base_input()).unwrap().id,
            remember(other).unwrap().id
        );
    }

    #[test]
    fn id_seed_matches_the_typescript_engine() {
        // Same seed the TS engine builds for this input (key empty ⇒ double space).
        let memory = remember(base_input()).unwrap();
        assert_eq!(memory.id, stable_id("user  hello chat agent", "mem"));
    }
}
