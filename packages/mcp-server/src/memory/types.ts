import { z } from 'zod';

/**
 * Typed agent memory (Part I.2). Mirrors the flat-file memory format Claude Code
 * already improvises (user | feedback | project | reference), but productized:
 * queryable, with mandatory provenance and conflict detection.
 */
export const MemoryTypeSchema = z.enum(['user', 'feedback', 'project', 'reference']);
export type MemoryType = z.infer<typeof MemoryTypeSchema>;

/** Provenance is mandatory: a fact without a source is not trustworthy. */
export const ProvenanceSchema = z.object({
  origin: z.string().min(1, 'provenance.origin must be a non-empty string'),
  author: z.enum(['human', 'agent']),
  timestamp: z.string().min(1),
});
export type Provenance = z.infer<typeof ProvenanceSchema>;

/** What a caller provides. `source.timestamp` may be omitted; remember() stamps it. */
export const MemoryInputSchema = z.object({
  content: z.string().min(1, 'content must be a non-empty string'),
  type: MemoryTypeSchema,
  tags: z.array(z.string()).default([]),
  /** Optional conflict key, e.g. "user.timezone". Same key + different content = conflict. */
  key: z.string().optional(),
  source: ProvenanceSchema,
  ttl: z.number().int().positive().optional(),
});
export type MemoryInput = z.infer<typeof MemoryInputSchema>;

/** A stored memory: input + a stable id. */
export const MemorySchema = MemoryInputSchema.extend({
  id: z.string().min(1),
});
export type Memory = z.infer<typeof MemorySchema>;
