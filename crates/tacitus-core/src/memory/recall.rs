use super::conflict::{detect_conflicts, Conflict};
use super::types::{Memory, MemoryType};
use crate::lexical::lexical_score;
use crate::tokens::estimate;

pub struct RecallArgs {
    pub query: String,
    pub memory_type: Option<MemoryType>,
    /// Hard ceiling: the sum of returned `token_count` never exceeds this.
    pub token_budget: Option<usize>,
}

pub struct RecallItem {
    pub memory: Memory,
    pub score: u32,
    pub token_count: usize,
}

pub struct RecallResult {
    pub items: Vec<RecallItem>,
    pub conflicts: Vec<Conflict>,
}

/// Rank relevant memories and return as many as fit under `token_budget`.
/// Budget is a hard ceiling: an item that doesn't fit is skipped, but smaller
/// ones are still tried. Conflicts are computed over the whole relevant set —
/// not the truncated items — so truncation can't hide a contradiction.
pub fn recall(memories: &[Memory], args: &RecallArgs) -> RecallResult {
    let mut ranked: Vec<(&Memory, u32)> = memories
        .iter()
        .filter(|m| args.memory_type.is_none_or(|t| m.memory_type == t))
        .map(|m| (m, lexical_score(&args.query, &m.content)))
        .filter(|(_, score)| *score > 0)
        .collect();
    ranked.sort_by_key(|r| std::cmp::Reverse(r.1));

    let budget = args.token_budget.unwrap_or(usize::MAX);
    let mut items = Vec::new();
    let mut used = 0usize;
    for (memory, score) in &ranked {
        let token_count = estimate(&memory.content);
        if used + token_count > budget {
            continue;
        }
        items.push(RecallItem {
            memory: (*memory).clone(),
            score: *score,
            token_count,
        });
        used += token_count;
    }

    let relevant: Vec<Memory> = ranked.iter().map(|(m, _)| (*m).clone()).collect();
    RecallResult {
        items,
        conflicts: detect_conflicts(&relevant),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::test_util::{keyed, mem, typed};

    fn args(query: &str, budget: Option<usize>) -> RecallArgs {
        RecallArgs {
            query: query.into(),
            memory_type: None,
            token_budget: budget,
        }
    }

    #[test]
    fn returns_only_relevant_ordered_descending() {
        let mems = vec![
            mem("a", "alpha beta gamma"),
            mem("b", "beta beta beta"),
            mem("c", "nothing here"),
        ];
        let res = recall(&mems, &args("beta", None));
        let ids: Vec<&str> = res.items.iter().map(|i| i.memory.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);
        assert!(res.items.iter().all(|i| i.score > 0));
    }

    #[test]
    fn never_exceeds_token_budget() {
        let mems = vec![
            mem("b", "beta beta beta"),
            mem("a", "alpha beta gamma delta"),
        ];
        let res = recall(&mems, &args("beta alpha", Some(5)));
        let total: usize = res.items.iter().map(|i| i.token_count).sum();
        assert!(total <= 5);
    }

    #[test]
    fn fewer_items_under_a_tighter_budget() {
        let mems = vec![
            mem("b", "beta beta beta"),
            mem("a", "alpha beta gamma delta"),
        ];
        let big = recall(&mems, &args("beta alpha", Some(1000))).items.len();
        let small = recall(&mems, &args("beta alpha", Some(3))).items.len();
        assert!(small < big);
    }

    #[test]
    fn filters_by_memory_type() {
        let mems = vec![
            typed("u", MemoryType::User, "preference alpha"),
            typed("p", MemoryType::Project, "preference alpha"),
        ];
        let res = recall(
            &mems,
            &RecallArgs {
                query: "preference".into(),
                memory_type: Some(MemoryType::Project),
                token_budget: None,
            },
        );
        let ids: Vec<&str> = res.items.iter().map(|i| i.memory.id.as_str()).collect();
        assert_eq!(ids, vec!["p"]);
    }

    #[test]
    fn surfaces_conflicts_instead_of_choosing() {
        let mems = vec![
            keyed("tz1", "user.timezone", "timezone Europe Bucharest"),
            keyed("tz2", "user.timezone", "timezone America New_York"),
        ];
        let res = recall(&mems, &args("timezone", None));
        assert_eq!(res.conflicts.len(), 1);
    }
}
