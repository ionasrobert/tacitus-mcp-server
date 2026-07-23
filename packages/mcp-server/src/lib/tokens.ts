/**
 * Token estimation.
 *
 * The token budget is the agent's scarcest resource (Part I.1). Every retrieval
 * respects it. The estimator is pluggable so we can later swap the cheap
 * heuristic for a real tokenizer without touching call sites.
 */
export interface TokenEstimator {
  estimate(text: string): number;
}

/** Cheap heuristic: ~4 characters per token. Good enough for budgeting. */
export const heuristicEstimator: TokenEstimator = {
  estimate(text: string): number {
    if (!text) return 0;
    return Math.ceil(text.length / 4);
  },
};

export function estimate(text: string, estimator: TokenEstimator = heuristicEstimator): number {
  return estimator.estimate(text);
}
