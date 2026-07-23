use std::collections::HashMap;

/// Shared lexical scoring, used by memory ranking and (later) vault search.
pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Score = summed term frequency of query terms within the text.
pub fn lexical_score(query: &str, text: &str) -> u32 {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for word in tokenize(text) {
        *counts.entry(word).or_insert(0) += 1;
    }
    tokenize(query)
        .iter()
        .map(|term| counts.get(term).copied().unwrap_or(0))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_by_term_frequency_case_insensitive() {
        assert_eq!(lexical_score("quick fox", "Quick fox quick fox"), 4);
        assert_eq!(lexical_score("quantum", "unrelated text"), 0);
    }
}
