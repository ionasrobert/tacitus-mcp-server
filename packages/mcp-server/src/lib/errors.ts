/**
 * Structured, actionable errors.
 *
 * Agent-facing rule (see CLAUDE.md, Part I.5): never surface a bare "failed".
 * Every error tells the agent what went wrong AND what to do differently.
 */
export interface StructuredError {
  code: string;
  reason: string;
  suggestion: string;
}

export class TacitusError extends Error {
  readonly code: string;
  readonly reason: string;
  readonly suggestion: string;

  constructor(error: StructuredError) {
    super(`${error.code}: ${error.reason}`);
    this.name = 'TacitusError';
    this.code = error.code;
    this.reason = error.reason;
    this.suggestion = error.suggestion;
  }

  toStructured(): StructuredError {
    return { code: this.code, reason: this.reason, suggestion: this.suggestion };
  }
}

/** Used by M0 stubs so tests fail on assertions, not on import errors. */
export class NotImplementedError extends TacitusError {
  constructor(what: string) {
    super({
      code: 'NOT_IMPLEMENTED',
      reason: `${what} is not implemented yet.`,
      suggestion: 'Implement the corresponding milestone to make this pass.',
    });
    this.name = 'NotImplementedError';
  }
}

export function toStructuredError(err: unknown): StructuredError {
  if (err instanceof TacitusError) return err.toStructured();
  return {
    code: 'INTERNAL',
    reason: err instanceof Error ? err.message : String(err),
    suggestion: 'This is an unexpected internal error; please report it.',
  };
}
