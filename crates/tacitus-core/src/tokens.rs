/// Token estimation — the agent's scarcest resource is the context window.
/// Cheap heuristic: ~4 characters per token (mirrors the TS `heuristicEstimator`).
pub fn estimate(text: &str) -> usize {
    let chars = text.chars().count();
    chars.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_ceil_of_len_over_four() {
        assert_eq!(estimate(""), 0);
        assert_eq!(estimate("abcd"), 1);
        assert_eq!(estimate("abcde"), 2);
    }
}
