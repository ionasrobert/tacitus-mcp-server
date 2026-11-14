import { describe, it, expect, afterEach } from 'vitest';
import { rm } from 'node:fs/promises';
import { HashingEmbedder } from '../../src/vault/embed';
import { VaultIndex } from '../../src/vault/index';
import { suggestLinks, type LinkSuggestion } from '../../src/vault/suggest';
import { vaultWith } from '../helpers/vault';

const dirs: string[] = [];
async function indexOf(notes: Record<string, string>): Promise<VaultIndex> {
  const files = Object.fromEntries(Object.entries(notes).map(([id, raw]) => [`${id}.md`, raw]));
  const dir = await vaultWith(files);
  dirs.push(dir);
  return VaultIndex.build(dir);
}
const ids = (s: LinkSuggestion[]): string[] => s.map((x) => x.note_id);
const embedder = new HashingEmbedder();

describe('suggest_links (mirrors suggest.rs)', () => {
  afterEach(async () => {
    for (const d of dirs.splice(0)) await rm(d, { recursive: true, force: true });
  });

  it('title mention outranks pure semantic', async () => {
    const index = await indexOf({
      daily:
        'Debugged the Kubernetes cluster today; the autoscaler kept evicting pods during deploys.',
      kubernetes: '# Kubernetes\n\nCluster operations: autoscaling, pod eviction, deploy runbooks.',
      containers: '# Containers\n\nCluster autoscaling and pod eviction during deploys, runbooks.',
    });
    const res = await suggestLinks(index, 'daily', embedder);
    expect(res[0]?.note_id).toBe('kubernetes');
    expect(res[0]?.reasons).toContain('title_mentioned');
  });

  it('excludes self and already-linked notes', async () => {
    const index = await indexOf({
      daily:
        'Worked on [[kubernetes]] today. Also drafted the Docker migration checklist for the cluster.',
      kubernetes: '# Kubernetes\n\nCluster ops.',
      docker: '# Docker\n\nContainer images and the migration checklist.',
    });
    const got = ids(await suggestLinks(index, 'daily', embedder));
    expect(got).not.toContain('daily');
    expect(got).not.toContain('kubernetes');
    expect(got).toContain('docker');
  });

  it('a backlinking note stays a candidate with the backlink reason', async () => {
    const index = await indexOf({
      project: 'Launch plan for the search feature: indexing, ranking, and rollout.',
      retro: 'Retro notes on the launch plan: indexing and ranking learnings. See [[project]].',
    });
    const retro = (await suggestLinks(index, 'project', embedder)).find(
      (s) => s.note_id === 'retro',
    );
    expect(retro?.reasons).toContain('backlink');
  });

  it('NOTE_NOT_FOUND for a missing source', async () => {
    const index = await indexOf({ a: 'Something.' });
    await expect(suggestLinks(index, 'nope', embedder)).rejects.toMatchObject({
      code: 'NOTE_NOT_FOUND',
    });
  });

  it('token_budget is a hard cap', async () => {
    const index = await indexOf({
      hub: 'Alpha ideas, Beta drafts, Gamma plans.',
      alpha: '# Alpha\n\nIdeas for the alpha track.',
      beta: '# Beta\n\nDrafts for the beta track.',
      gamma: '# Gamma\n\nPlans for the gamma track.',
    });
    const res = await suggestLinks(index, 'hub', embedder, { token_budget: 15 });
    expect(res.length).toBeGreaterThan(0);
    expect(res.length).toBeLessThan(3);
    expect(res.reduce((n, s) => n + s.token_count, 0)).toBeLessThanOrEqual(15);
  });

  it('min_score and top_k bound the output', async () => {
    const index = await indexOf({
      daily: 'Reviewed the Alpha spec and the Beta rollout.',
      alpha: '# Alpha\n\nSpec details.',
      beta: '# Beta\n\nRollout details.',
      gardening: '# Gardening\n\nPrune roses in spring; water tomatoes at dawn.',
    });
    expect(ids(await suggestLinks(index, 'daily', embedder))).not.toContain('gardening');
    expect(await suggestLinks(index, 'daily', embedder, { top_k: 1 })).toHaveLength(1);
  });

  it('shared tags surface as a reason', async () => {
    const index = await indexOf({
      a: '---\ntitle: Sprint planning\ntags: [project, q3]\n---\n\nPlanning the sprint backlog and estimates.',
      b: '---\ntitle: Sprint retro\ntags: [project, q3]\n---\n\nRetro on the sprint backlog and estimates.',
    });
    const b = (await suggestLinks(index, 'a', embedder)).find((s) => s.note_id === 'b');
    expect(b?.reasons).toContain('shared_tags');
  });

  it('a mention snippet shows source context', async () => {
    const index = await indexOf({
      journal:
        'Morning standup, then code review for the sync branch. The Roadmap review went well after lunch, we cut two items.',
      roadmap: '# Roadmap\n\nQuarters and milestones.',
    });
    const road = (await suggestLinks(index, 'journal', embedder)).find(
      (s) => s.note_id === 'roadmap',
    );
    expect(road?.reasons).toContain('title_mentioned');
    expect(road?.snippet).toContain('went well');
    expect(road?.snippet).not.toContain('milestones');
  });
});
