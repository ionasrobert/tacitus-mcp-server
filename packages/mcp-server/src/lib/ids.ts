import { createHash } from 'node:crypto';

/**
 * Deterministic, stable id from a seed string.
 *
 * Stable ids are the backbone of idempotency (Part I.4): re-remembering the
 * same fact from the same source yields the same id, so no duplicates.
 */
export function stableId(seed: string, prefix = 'mem'): string {
  const digest = createHash('sha256').update(seed).digest('hex').slice(0, 16);
  return `${prefix}_${digest}`;
}
