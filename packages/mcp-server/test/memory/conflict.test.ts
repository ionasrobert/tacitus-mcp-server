import { describe, it, expect } from 'vitest';
import { detectConflicts } from '../../src/memory/conflict';
import { recall } from '../../src/memory/recall';
import { makeMemory } from '../helpers/fixtures';

describe('detectConflicts (M3)', () => {
  it('flags memories with the same key but different content', () => {
    const mems = [
      makeMemory({ id: 'tz1', key: 'user.timezone', content: 'Europe/Bucharest' }),
      makeMemory({ id: 'tz2', key: 'user.timezone', content: 'America/New_York' }),
    ];
    const conflicts = detectConflicts(mems);
    expect(conflicts).toHaveLength(1);
    expect(conflicts[0]?.key).toBe('user.timezone');
    expect([...(conflicts[0]?.memoryIds ?? [])].sort()).toEqual(['tz1', 'tz2']);
  });

  it('does not flag agreeing memories (same key, same content)', () => {
    const mems = [
      makeMemory({ id: 'a', key: 'user.name', content: 'Robert' }),
      makeMemory({ id: 'b', key: 'user.name', content: 'Robert' }),
    ];
    expect(detectConflicts(mems)).toEqual([]);
  });

  it('ignores memories without a key', () => {
    const mems = [makeMemory({ id: 'a', content: 'foo' }), makeMemory({ id: 'b', content: 'bar' })];
    expect(detectConflicts(mems)).toEqual([]);
  });

  it('recall surfaces conflicts instead of silently choosing', () => {
    const mems = [
      makeMemory({ id: 'tz1', key: 'user.timezone', content: 'timezone is Europe Bucharest' }),
      makeMemory({ id: 'tz2', key: 'user.timezone', content: 'timezone is America New_York' }),
    ];
    const res = recall(mems, { query: 'timezone' });
    expect(res.conflicts).toHaveLength(1);
    expect([...(res.conflicts[0]?.memoryIds ?? [])].sort()).toEqual(['tz1', 'tz2']);
  });
});
