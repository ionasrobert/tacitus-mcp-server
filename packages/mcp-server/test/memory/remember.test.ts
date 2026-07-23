import { describe, it, expect } from 'vitest';
import { remember } from '../../src/memory/remember';
import { TacitusError } from '../../src/lib/errors';

const validSource = { origin: 'chat', author: 'agent' as const, timestamp: '2026-07-23T10:00:00.000Z' };

describe('remember (M1: types + mandatory provenance)', () => {
  it('rejects input without a source (MISSING_PROVENANCE)', () => {
    try {
      remember({ content: 'hello', type: 'user' });
      throw new Error('expected remember to throw');
    } catch (err) {
      expect(err).toBeInstanceOf(TacitusError);
      expect((err as TacitusError).code).toBe('MISSING_PROVENANCE');
    }
  });

  it('rejects an invalid memory type (INVALID_TYPE)', () => {
    try {
      remember({ content: 'hello', type: 'nonsense', source: validSource });
      throw new Error('expected remember to throw');
    } catch (err) {
      expect(err).toBeInstanceOf(TacitusError);
      expect((err as TacitusError).code).toBe('INVALID_TYPE');
    }
  });

  it('assigns a stable id and stamps a timestamp when omitted', () => {
    const mem = remember({
      content: 'the launch is in March',
      type: 'project',
      source: { origin: 'chat', author: 'agent' },
    });
    expect(mem.id).toMatch(/^mem_[0-9a-f]{16}$/);
    expect(mem.source.timestamp).toBeTruthy();
    expect(Number.isNaN(Date.parse(mem.source.timestamp))).toBe(false);
  });

  it('is idempotent: identical content+source ⇒ identical id', () => {
    const a = remember({ content: 'the sky is blue', type: 'user', key: 'sky.color', source: validSource });
    const b = remember({ content: 'the sky is blue', type: 'user', key: 'sky.color', source: validSource });
    expect(a.id).toBe(b.id);
  });

  it('different content ⇒ different id', () => {
    const a = remember({ content: 'fact A', type: 'user', source: validSource });
    const b = remember({ content: 'fact B', type: 'user', source: validSource });
    expect(a.id).not.toBe(b.id);
  });
});
