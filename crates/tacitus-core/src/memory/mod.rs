pub mod conflict;
pub mod rank;
pub mod recall;
pub mod remember;
pub mod store;
pub mod types;

pub use conflict::{detect_conflicts, Conflict};
pub use recall::{recall, RecallArgs, RecallItem, RecallResult};
pub use remember::{remember, ProvenanceInput, RememberInput};
pub use store::MemoryStore;
pub use types::{Author, Memory, MemoryType, Provenance};

#[cfg(test)]
pub(crate) mod test_util {
    use super::types::{Author, Memory, MemoryType, Provenance};

    pub fn mem(id: &str, content: &str) -> Memory {
        Memory {
            id: id.into(),
            memory_type: MemoryType::User,
            content: content.into(),
            tags: vec![],
            key: None,
            source: Provenance {
                origin: "test".into(),
                author: Author::Agent,
                timestamp: "2026-07-23T10:00:00Z".into(),
            },
            ttl: None,
        }
    }

    pub fn keyed(id: &str, key: &str, content: &str) -> Memory {
        let mut memory = mem(id, content);
        memory.key = Some(key.into());
        memory
    }

    pub fn typed(id: &str, memory_type: MemoryType, content: &str) -> Memory {
        let mut memory = mem(id, content);
        memory.memory_type = memory_type;
        memory
    }
}
