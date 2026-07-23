import { lexicalScore, tokenize } from '../lib/lexical';
import { estimate } from '../lib/tokens';
import { cosine, HashingEmbedder, type Embedder } from './embed';
import type { VaultIndex } from './index';

export type SearchMode = 'lexical' | 'semantic' | 'hybrid';

export interface SearchArgs {
  mode?: SearchMode;
  token_budget?: number;
  top_k?: number;
  embedder?: Embedder;
}

export interface SearchHit {
  note_id: string;
  title: string;
  score: number;
  snippet: string;
  token_count: number;
}

export interface SearchResult {
  hits: SearchHit[];
}

const SNIPPET_MAX = 240;
/** Minimum semantic similarity for a note with no lexical match to be recalled. */
const SEMANTIC_MIN_SIM = 0.15;
const defaultEmbedder = new HashingEmbedder();

function makeSnippet(query: string, content: string): string {
  const terms = new Set(tokenize(query));
  const collapsed = content.replace(/\s+/g, ' ').trim();
  const lower = collapsed.toLowerCase();
  let at = -1;
  for (const term of terms) {
    const idx = lower.indexOf(term);
    if (idx >= 0 && (at < 0 || idx < at)) at = idx;
  }
  const start = at < 0 ? 0 : Math.max(0, at - 40);
  return collapsed.slice(start, start + SNIPPET_MAX);
}

/**
 * Rank notes by relevance and return snippets under a token budget (Part I.1).
 * mode: lexical (exact terms), semantic (embedding cosine), or hybrid (blend —
 * the default: lexical precision plus semantic recall for variants/paraphrases).
 */
export async function searchNotes(
  index: VaultIndex,
  query: string,
  args: SearchArgs = {},
): Promise<SearchResult> {
  const mode = args.mode ?? 'hybrid';
  const embedder = args.embedder ?? defaultEmbedder;
  const notes = index.all();

  const lexical = new Map<string, number>();
  let maxLexical = 0;
  for (const note of notes) {
    const score = lexicalScore(query, `${note.title} ${note.content}`);
    lexical.set(note.id, score);
    if (score > maxLexical) maxLexical = score;
  }

  const semantic = new Map<string, number>();
  if (mode !== 'lexical') {
    const vecs = await embedder.embedBatch([
      query,
      ...notes.map((note) => `${note.title} ${note.content}`),
    ]);
    const queryVec = vecs[0] ?? [];
    notes.forEach((note, i) => {
      semantic.set(note.id, Math.max(0, cosine(queryVec, vecs[i + 1] ?? [])));
    });
  }

  const ranked = notes
    .map((note) => {
      const lex = lexical.get(note.id) ?? 0;
      const sem = semantic.get(note.id) ?? 0;
      let score: number;
      let include: boolean;
      if (mode === 'lexical') {
        score = lex;
        include = lex > 0;
      } else if (mode === 'semantic') {
        score = sem;
        include = sem >= SEMANTIC_MIN_SIM;
      } else {
        const lexNorm = maxLexical > 0 ? lex / maxLexical : 0;
        score = 0.5 * lexNorm + 0.5 * sem;
        include = lex > 0 || sem >= SEMANTIC_MIN_SIM;
      }
      return { note, score, include };
    })
    .filter((r) => r.include)
    .sort((a, b) => b.score - a.score);

  const capped = args.top_k !== undefined ? ranked.slice(0, args.top_k) : ranked;
  const budget = args.token_budget ?? Number.POSITIVE_INFINITY;

  const hits: SearchHit[] = [];
  let used = 0;
  for (const { note, score } of capped) {
    const snippet = makeSnippet(query, note.content);
    const token_count = estimate(snippet);
    if (used + token_count > budget) continue;
    hits.push({ note_id: note.id, title: note.title, score, snippet, token_count });
    used += token_count;
  }

  return { hits };
}
