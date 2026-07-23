/** Shared lexical scoring used by memory ranking and vault search. */
export function tokenize(text: string): string[] {
  return text.toLowerCase().match(/[a-z0-9]+/g) ?? [];
}

/** Score = summed term frequency of query terms within the text. */
export function lexicalScore(query: string, text: string): number {
  const counts = new Map<string, number>();
  for (const word of tokenize(text)) {
    counts.set(word, (counts.get(word) ?? 0) + 1);
  }
  let score = 0;
  for (const term of tokenize(query)) score += counts.get(term) ?? 0;
  return score;
}
