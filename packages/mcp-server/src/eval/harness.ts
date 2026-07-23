import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { searchNotes } from '../vault/search';
import { VaultIndex } from '../vault/index';
import { setScore, tokenEfficiency, type SetScore } from './metrics';

export interface SeedNote {
  id: string;
  content: string;
}

export interface RetrievalScenario {
  name: string;
  notes: SeedNote[];
  query: string;
  relevant: string[];
  token_budget?: number;
  top_k?: number;
}

export interface ScenarioResult {
  name: string;
  retrieved: string[];
  scores: SetScore;
  tokenEfficiency: number;
  tokensReturned: number;
  tokenBudget?: number;
}

export interface SuiteResult {
  scenarios: ScenarioResult[];
  avg: { precision: number; recall: number; f1: number; tokenEfficiency: number };
}

/**
 * Retriever seam: swap the default lexical search for a Claude-driven agent
 * (which calls the MCP tools) when ANTHROPIC_API_KEY is available. The scoring
 * stays identical, so LLM and deterministic runs are directly comparable.
 */
export type Retriever = (
  index: VaultIndex,
  query: string,
  opts: { token_budget?: number; top_k?: number },
) => Promise<{ note_id: string; token_count: number }[]>;

const defaultRetriever: Retriever = async (index, query, opts) =>
  (await searchNotes(index, query, opts)).hits.map((h) => ({
    note_id: h.note_id,
    token_count: h.token_count,
  }));

async function materialize(notes: SeedNote[]): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), 'mv-eval-'));
  for (const note of notes) {
    const path = join(dir, `${note.id}.md`);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, note.content, 'utf8');
  }
  return dir;
}

export async function runRetrievalScenario(
  scenario: RetrievalScenario,
  retrieve: Retriever = defaultRetriever,
): Promise<ScenarioResult> {
  const dir = await materialize(scenario.notes);
  try {
    const index = await VaultIndex.build(dir);
    const hits = await retrieve(index, scenario.query, {
      token_budget: scenario.token_budget,
      top_k: scenario.top_k,
    });
    const retrieved = hits.map((h) => h.note_id);
    const items = hits.map((h) => ({ id: h.note_id, token_count: h.token_count }));
    return {
      name: scenario.name,
      retrieved,
      scores: setScore(retrieved, scenario.relevant),
      tokenEfficiency: tokenEfficiency(items, scenario.relevant),
      tokensReturned: items.reduce((sum, i) => sum + i.token_count, 0),
      tokenBudget: scenario.token_budget,
    };
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

export async function runSuite(
  scenarios: RetrievalScenario[],
  retrieve: Retriever = defaultRetriever,
): Promise<SuiteResult> {
  const results: ScenarioResult[] = [];
  for (const scenario of scenarios) results.push(await runRetrievalScenario(scenario, retrieve));

  const n = results.length || 1;
  const avg = {
    precision: results.reduce((a, r) => a + r.scores.precision, 0) / n,
    recall: results.reduce((a, r) => a + r.scores.recall, 0) / n,
    f1: results.reduce((a, r) => a + r.scores.f1, 0) / n,
    tokenEfficiency: results.reduce((a, r) => a + r.tokenEfficiency, 0) / n,
  };
  return { scenarios: results, avg };
}
