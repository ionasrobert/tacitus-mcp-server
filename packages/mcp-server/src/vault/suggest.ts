import { TacitusError } from '../lib/errors';
import { tokenize } from '../lib/lexical';
import { estimate } from '../lib/tokens';
import { cosine, HashingEmbedder, type Embedder } from './embed';
import type { VaultIndex } from './index';
import { makeSnippet } from './search';

/**
 * Suggest the [[wikilinks]] a note is missing: rank unlinked candidates by
 * title mentions in the source, embedding similarity, shared tags, and
 * existing backlinks — each with machine-readable reasons. Pure over the
 * index; output bounded like search.
 * Port of crates/tacitus-core/src/vault/suggest.rs (same weights/thresholds).
 */

const DEFAULT_TOP_K = 5;
/** Below this blended score a candidate is noise, not a suggestion. */
const DEFAULT_MIN_SCORE = 0.15;
/** A mention alone (0.45) outranks any realistic pure-semantic score (≤0.30). */
const MENTION_WEIGHT = 0.45;
const SEM_WEIGHT = 0.3;
const TAG_WEIGHT = 0.15;
/** A note that already links here nudges toward making the link bidirectional. */
const BACKLINK_WEIGHT = 0.1;
/** Cosine below this doesn't count as a "semantic" reason. */
const SEMANTIC_REASON_MIN = 0.15;
/** Shared tags beyond this add no more signal. */
const TAG_CAP = 3;

const defaultEmbedder = new HashingEmbedder();

export interface SuggestArgs {
  top_k?: number;
  min_score?: number;
  token_budget?: number;
}

export interface LinkSuggestion {
  note_id: string;
  title: string;
  score: number;
  /** "title_mentioned" | "semantic" | "shared_tags" | "backlink" */
  reasons: string[];
  snippet: string;
  token_count: number;
}

/**
 * True when the candidate's title (or id basename) appears as a contiguous
 * token window in the source — token matching, so "Art" never fires inside
 * "start" and case/punctuation don't matter.
 */
function mentions(sourceTokens: string[], title: string, basename: string): boolean {
  for (const needle of [tokenize(title), tokenize(basename)]) {
    if (needle.length === 0 || needle.reduce((n, t) => n + t.length, 0) < 3) continue;
    for (let i = 0; i + needle.length <= sourceTokens.length; i += 1) {
      if (needle.every((t, j) => sourceTokens[i + j] === t)) return true;
    }
  }
  return false;
}

export async function suggestLinks(
  index: VaultIndex,
  noteId: string,
  embedder: Embedder = defaultEmbedder,
  args: SuggestArgs = {},
): Promise<LinkSuggestion[]> {
  const source = index.get(noteId);
  if (!source) {
    throw new TacitusError({
      code: 'NOTE_NOT_FOUND',
      reason: `No note with id "${noteId}".`,
      suggestion: 'Use list_notes to discover valid note ids.',
    });
  }

  const linked = new Set(index.outgoing(noteId).map((n) => n.id));
  const backlinkers = new Set(index.backlinks(noteId).map((n) => n.id));
  const candidates = index.all().filter((n) => n.id !== noteId && !linked.has(n.id));

  const vecs = await embedder.embedBatch([
    `${source.title} ${source.content}`,
    ...candidates.map((n) => `${n.title} ${n.content}`),
  ]);
  const sourceVec = vecs[0] ?? [];
  const sourceTokens = tokenize(source.content);
  const sourceTags = new Set(source.tags);
  const minScore = args.min_score ?? DEFAULT_MIN_SCORE;

  const scored: {
    note: (typeof candidates)[number];
    score: number;
    reasons: string[];
    mentioned: boolean;
  }[] = [];
  candidates.forEach((note, i) => {
    const sem = Math.max(0, cosine(sourceVec, vecs[i + 1] ?? []));
    const basename = note.id.split('/').pop() ?? note.id;
    const mentioned = mentions(sourceTokens, note.title, basename);
    const sharedTags = note.tags.filter((t) => sourceTags.has(t)).length;
    const backlink = backlinkers.has(note.id);

    const score =
      (mentioned ? MENTION_WEIGHT : 0) +
      SEM_WEIGHT * sem +
      TAG_WEIGHT * (Math.min(sharedTags, TAG_CAP) / TAG_CAP) +
      (backlink ? BACKLINK_WEIGHT : 0);
    if (score < minScore) return;

    const reasons: string[] = [];
    if (mentioned) reasons.push('title_mentioned');
    if (sem >= SEMANTIC_REASON_MIN) reasons.push('semantic');
    if (sharedTags > 0) reasons.push('shared_tags');
    if (backlink) reasons.push('backlink');
    scored.push({ note, score, reasons, mentioned });
  });

  scored.sort(
    (a, b) => b.score - a.score || (a.note.id < b.note.id ? -1 : a.note.id > b.note.id ? 1 : 0),
  );
  const top = scored.slice(0, args.top_k ?? DEFAULT_TOP_K);

  const budget = args.token_budget ?? Number.POSITIVE_INFINITY;
  const suggestions: LinkSuggestion[] = [];
  let used = 0;
  for (const { note, score, reasons, mentioned } of top) {
    // A mention's snippet shows WHERE in the source the link would go;
    // otherwise show what the candidate is about.
    const snippet = mentioned
      ? makeSnippet(note.title, source.content)
      : makeSnippet(source.title, note.content);
    const token_count = estimate(snippet);
    if (used + token_count > budget) continue;
    suggestions.push({ note_id: note.id, title: note.title, score, reasons, snippet, token_count });
    used += token_count;
  }
  return suggestions;
}
