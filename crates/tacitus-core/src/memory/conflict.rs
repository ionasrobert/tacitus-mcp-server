use std::collections::{HashMap, HashSet};

use super::types::Memory;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub key: String,
    pub memory_ids: Vec<String>,
}

/// Detect contradicting memories: same `key`, different `content`.
/// Surfaced rather than silently resolved. Memories without a key are ignored.
pub fn detect_conflicts(memories: &[Memory]) -> Vec<Conflict> {
    let mut by_key: HashMap<&str, Vec<&Memory>> = HashMap::new();
    for memory in memories {
        if let Some(key) = &memory.key {
            by_key.entry(key).or_default().push(memory);
        }
    }

    let mut conflicts = Vec::new();
    for (key, group) in by_key {
        let distinct: HashSet<&str> = group.iter().map(|m| m.content.as_str()).collect();
        if distinct.len() > 1 {
            conflicts.push(Conflict {
                key: key.to_string(),
                memory_ids: group.iter().map(|m| m.id.clone()).collect(),
            });
        }
    }
    conflicts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::test_util::{keyed, mem};

    #[test]
    fn flags_same_key_different_content() {
        let mems = vec![
            keyed("tz1", "user.timezone", "Europe/Bucharest"),
            keyed("tz2", "user.timezone", "America/New_York"),
        ];
        let conflicts = detect_conflicts(&mems);
        assert_eq!(conflicts.len(), 1);
        let mut ids = conflicts[0].memory_ids.clone();
        ids.sort();
        assert_eq!(ids, vec!["tz1", "tz2"]);
    }

    #[test]
    fn does_not_flag_agreeing_memories() {
        let mems = vec![keyed("a", "k", "same"), keyed("b", "k", "same")];
        assert!(detect_conflicts(&mems).is_empty());
    }

    #[test]
    fn ignores_memories_without_a_key() {
        let mems = vec![mem("a", "foo"), mem("b", "bar")];
        assert!(detect_conflicts(&mems).is_empty());
    }
}
