export interface SetScore {
  precision: number;
  recall: number;
  f1: number;
}

/** Precision/recall/F1 of a retrieved id set against the relevant id set. */
export function setScore(retrieved: string[], relevant: string[]): SetScore {
  const rel = new Set(relevant);
  const ret = new Set(retrieved);
  const truePositives = [...ret].filter((id) => rel.has(id)).length;

  const precision = ret.size === 0 ? (rel.size === 0 ? 1 : 0) : truePositives / ret.size;
  const recall = rel.size === 0 ? 1 : truePositives / rel.size;
  const f1 = precision + recall === 0 ? 0 : (2 * precision * recall) / (precision + recall);
  return { precision, recall, f1 };
}

/** Fraction of returned tokens that belong to relevant items (higher = less waste). */
export function tokenEfficiency(
  items: { id: string; token_count: number }[],
  relevant: string[],
): number {
  const rel = new Set(relevant);
  const total = items.reduce((sum, i) => sum + i.token_count, 0);
  if (total === 0) return 1;
  const relevantTokens = items
    .filter((i) => rel.has(i.id))
    .reduce((sum, i) => sum + i.token_count, 0);
  return relevantTokens / total;
}
