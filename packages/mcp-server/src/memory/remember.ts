import { TacitusError } from '../lib/errors';
import { stableId } from '../lib/ids';
import { MemoryInputSchema, MemoryTypeSchema, type Memory } from './types';

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/**
 * Validate an input, enforce mandatory provenance, and return a stored Memory
 * with a stable id. Idempotent: identical content+source ⇒ identical id
 * (the id seed excludes the timestamp, so re-remembering a fact is a no-op).
 */
export function remember(input: unknown): Memory {
  if (!isObject(input)) {
    throw new TacitusError({
      code: 'INVALID_INPUT',
      reason: 'remember() expects an object.',
      suggestion: 'Pass { content, type, source, tags?, key?, ttl? }.',
    });
  }

  if (!isObject(input.source)) {
    throw new TacitusError({
      code: 'MISSING_PROVENANCE',
      reason: 'A memory must carry its source (provenance).',
      suggestion: 'Provide source: { origin, author: "human"|"agent", timestamp? }.',
    });
  }

  // Validate type early so the caller gets a precise INVALID_TYPE, not a generic schema error.
  if (!MemoryTypeSchema.safeParse(input.type).success) {
    throw new TacitusError({
      code: 'INVALID_TYPE',
      reason: `type must be one of user|feedback|project|reference (got ${JSON.stringify(input.type)}).`,
      suggestion: 'Use a valid memory type.',
    });
  }

  // Stamp the provenance timestamp when the caller omits it.
  const source = input.source as Record<string, unknown>;
  const stamped = {
    ...input,
    source: {
      ...source,
      timestamp:
        typeof source.timestamp === 'string' && source.timestamp.length > 0
          ? source.timestamp
          : new Date().toISOString(),
    },
  };

  const parsed = MemoryInputSchema.safeParse(stamped);
  if (!parsed.success) {
    const issue = parsed.error.issues[0];
    throw new TacitusError({
      code: 'INVALID_INPUT',
      reason: issue
        ? `${issue.path.join('.') || '(root)'}: ${issue.message}`
        : 'Invalid memory input.',
      suggestion: 'Fix the reported field and retry.',
    });
  }

  const memory = parsed.data;
  const seed = [
    memory.type,
    memory.key ?? '',
    memory.content,
    memory.source.origin,
    memory.source.author,
  ].join(' ');
  return { id: stableId(seed), ...memory };
}
