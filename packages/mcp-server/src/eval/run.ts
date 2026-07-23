import { runSuite } from './harness';
import { RETRIEVAL_SCENARIOS } from './scenarios';

/** Human-facing eval report: `npm run eval`. CI gates via harness.test.ts. */
async function main(): Promise<void> {
  const suite = await runSuite(RETRIEVAL_SCENARIOS);
  for (const s of suite.scenarios) {
    process.stdout.write(
      `f1=${s.scores.f1.toFixed(2)}  eff=${s.tokenEfficiency.toFixed(2)}  tok=${s.tokensReturned}  ${s.name}\n` +
        `    retrieved: ${s.retrieved.join(', ') || '(none)'}\n`,
    );
  }
  const { precision, recall, f1, tokenEfficiency } = suite.avg;
  process.stdout.write(
    `\nAVG  precision=${precision.toFixed(2)}  recall=${recall.toFixed(2)}  f1=${f1.toFixed(2)}  tokenEff=${tokenEfficiency.toFixed(2)}\n`,
  );
}

main().catch((err: unknown) => {
  process.stderr.write(`eval failed: ${err instanceof Error ? err.message : String(err)}\n`);
  process.exit(1);
});
