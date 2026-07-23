use super::types::Memory;
use crate::lexical::lexical_score;

/// Rank memories by lexical relevance to the query. Returns `(index, score)`
/// pairs sorted by descending score (every memory, including score 0).
pub fn rank(query: &str, memories: &[Memory]) -> Vec<(usize, u32)> {
    let mut scored: Vec<(usize, u32)> = memories
        .iter()
        .enumerate()
        .map(|(i, m)| (i, lexical_score(query, &m.content)))
        .collect();
    scored.sort_by_key(|s| std::cmp::Reverse(s.1));
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::test_util::mem;
    use std::collections::HashMap;

    #[test]
    fn higher_term_frequency_scores_higher() {
        let mems = vec![
            mem("m1", "the quick brown fox"),
            mem("m2", "quick fox quick fox"),
        ];
        let scores: HashMap<usize, u32> = rank("quick fox", &mems).into_iter().collect();
        assert!(scores[&1] > scores[&0]);
    }

    #[test]
    fn results_are_sorted_descending() {
        let mems = vec![
            mem("a", "apple"),
            mem("b", "apple apple"),
            mem("c", "banana"),
        ];
        let scores: Vec<u32> = rank("apple", &mems).into_iter().map(|(_, s)| s).collect();
        let mut sorted = scores.clone();
        sorted.sort_by_key(|&s| std::cmp::Reverse(s));
        assert_eq!(scores, sorted);
    }
}
