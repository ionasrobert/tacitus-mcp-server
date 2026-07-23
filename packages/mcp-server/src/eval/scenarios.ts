import type { RetrievalScenario } from './harness';

/** Deterministic retrieval scenarios that gate PRs on agent-experience quality. */
export const RETRIEVAL_SCENARIOS: RetrievalScenario[] = [
  {
    name: 'finds the note about the launch deadline among distractors',
    notes: [
      { id: 'launch', content: '# Launch\nThe product launch deadline is March 2026.' },
      { id: 'recipes', content: '# Recipes\nHow to bake sourdough bread at home.' },
      { id: 'budget', content: '# Budget\nQuarterly financial planning and spreadsheets.' },
    ],
    query: 'launch deadline',
    relevant: ['launch'],
  },
  {
    name: 'finds both notes mentioning the kubernetes cluster',
    notes: [
      { id: 'infra/k8s', content: 'Kubernetes cluster autoscaling notes.' },
      { id: 'infra/deploy', content: 'Deploying services to the kubernetes cluster.' },
      { id: 'marketing', content: 'Social media campaign ideas for Q2.' },
    ],
    query: 'kubernetes cluster',
    relevant: ['infra/k8s', 'infra/deploy'],
  },
  {
    name: 'ranks the on-topic note above a keyword-sharing distractor',
    notes: [
      { id: 'target', content: 'Migration plan: migrate the database schema safely.' },
      { id: 'distractor', content: 'Bird migration patterns in spring.' },
      { id: 'other', content: 'Coffee brewing methods and grind sizes.' },
    ],
    query: 'database migration',
    relevant: ['target'],
  },
  {
    name: 'stays within a tight token budget',
    notes: [
      { id: 'a', content: 'alpha alpha alpha signal signal signal' },
      { id: 'b', content: 'alpha signal ' + 'noise '.repeat(40) },
      { id: 'c', content: 'entirely unrelated gardening content' },
    ],
    query: 'alpha signal',
    relevant: ['a', 'b'],
    token_budget: 120,
  },
];
