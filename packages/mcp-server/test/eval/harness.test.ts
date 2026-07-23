import { describe, it, expect } from 'vitest';
import { runSuite } from '../../src/eval/harness';
import { RETRIEVAL_SCENARIOS } from '../../src/eval/scenarios';

// This is the CI "smoke eval": it scores the retrieval *experience* an agent
// gets, and gates PRs on quality thresholds — not just that the code runs.
describe('retrieval eval (M8: agent-experience gate)', () => {
  it('meets retrieval quality thresholds across scenarios', async () => {
    const suite = await runSuite(RETRIEVAL_SCENARIOS);
    expect(suite.avg.precision).toBeGreaterThanOrEqual(0.8);
    expect(suite.avg.recall).toBeGreaterThanOrEqual(0.9);
    expect(suite.avg.f1).toBeGreaterThanOrEqual(0.8);
    expect(suite.avg.tokenEfficiency).toBeGreaterThanOrEqual(0.5);
  });

  it('never exceeds a scenario token budget', async () => {
    const suite = await runSuite(RETRIEVAL_SCENARIOS);
    for (const s of suite.scenarios) {
      if (s.tokenBudget !== undefined) {
        expect(s.tokensReturned).toBeLessThanOrEqual(s.tokenBudget);
      }
    }
  });

  it('every scenario retrieves at least one relevant note', async () => {
    const suite = await runSuite(RETRIEVAL_SCENARIOS);
    for (const s of suite.scenarios) {
      expect(s.scores.recall).toBeGreaterThan(0);
    }
  });
});
