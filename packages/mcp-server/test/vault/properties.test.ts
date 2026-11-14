import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { rm } from 'node:fs/promises';
import { VaultIndex } from '../../src/vault/index';
import { propertiesQuery, type PropertiesRow } from '../../src/vault/properties';
import { vaultWith } from '../helpers/vault';

const ids = (rows: PropertiesRow[]): string[] => rows.map((r) => r.note_id);

describe('properties_query (mirrors properties.rs)', () => {
  let dir: string;
  let index: VaultIndex;
  beforeEach(async () => {
    dir = await vaultWith({
      'alpha.md':
        '---\nstatus: active\npriority: 3\ndue: 2026-08-01\ntags: [project, q3]\n---\nAlpha.\n',
      'beta.md': '---\nstatus: done\npriority: 1\ndue: 2026-07-01\ntags: [project]\n---\nBeta.\n',
      'gamma.md': '---\nstatus: active\npriority: 2\n---\nGamma (no due, no tags).\n',
      'plain.md': 'No frontmatter at all.\n',
    });
    index = await VaultIndex.build(dir);
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('eq and array contains are AND-ed', () => {
    const rows = propertiesQuery(index, {
      filters: [
        { key: 'status', op: 'eq', value: 'active' },
        { key: 'tags', op: 'contains', value: 'project' },
      ],
    });
    expect(ids(rows)).toEqual(['alpha']);
  });

  it('numeric comparison, sort and limit', () => {
    const rows = propertiesQuery(index, {
      filters: [{ key: 'priority', op: 'gte', value: 1 }],
      sort_by: 'priority',
      descending: true,
      limit: 2,
    });
    expect(ids(rows)).toEqual(['alpha', 'gamma']);
  });

  it('ISO dates stay strings (yaml core schema, like serde_yaml) and compare lexicographically', () => {
    expect(typeof index.get('alpha')!.frontmatter.due).toBe('string');
    const rows = propertiesQuery(index, {
      filters: [{ key: 'due', op: 'lt', value: '2026-07-15' }],
    });
    expect(ids(rows)).toEqual(['beta']);
  });

  it('exists / not_exists and select projection', () => {
    const rows = propertiesQuery(index, {
      filters: [
        { key: 'status', op: 'exists' },
        { key: 'due', op: 'not_exists' },
      ],
      select: ['status'],
    });
    expect(ids(rows)).toEqual(['gamma']);
    expect(rows[0]!.properties).toEqual({ status: 'active' });
  });

  it('substring contains is case-insensitive; ne excludes', () => {
    expect(
      ids(propertiesQuery(index, { filters: [{ key: 'status', op: 'contains', value: 'ACT' }] })),
    ).toEqual(['alpha', 'gamma']);
    expect(
      ids(propertiesQuery(index, { filters: [{ key: 'status', op: 'ne', value: 'active' }] })),
    ).toEqual(['beta']);
  });

  it('token_budget is a hard ceiling', () => {
    const all = propertiesQuery(index);
    expect(all).toHaveLength(4); // includes the empty-frontmatter note
    const total = all.reduce((n, r) => n + r.token_count, 0);
    const capped = propertiesQuery(index, { token_budget: Math.floor(total / 2) });
    expect(capped.length).toBeGreaterThan(0);
    expect(capped.length).toBeLessThan(all.length);
    expect(capped.reduce((n, r) => n + r.token_count, 0)).toBeLessThanOrEqual(total / 2);
  });
});
